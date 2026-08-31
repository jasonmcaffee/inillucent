# rust-db

A vector search engine for retrieval augmented generation, built as a Rust library that runs inside your own process. No database to install, no server to keep alive, no port to configure, and no network hop between your application and its index.

Retrieval augmented generation, usually shortened to RAG, means giving a language model the specific passages it needs to answer a question instead of hoping the answer is in its weights. The quality of the answer is capped by the quality of the retrieval, which is why the retrieval deserves an engine built for it.

## What it is for

rust-db is built for the searches an AI application actually performs, not for the general case of storing vectors.

- **Answering a question over a body of written knowledge.** Find the passages that mean the same thing as the question, even when they share no words with it.
- **Finding an exact token.** A user who types `PROJ-1932` or `parse_headers` wants that identifier, not passages about vaguely similar ones. Meaning based search is bad at this and word based search is good at it, so rust-db does both and merges the results.
- **Searching one slice of the corpus.** An agent with a tool per source needs a search restricted to chat messages, or to one repository, or to one author, or to everything updated since June. This is the common case in a real application and it is where general purpose vector storage struggles hardest.
- **Answering from a process that starts and stops.** Reopening a saved index takes 5.3 seconds against 175 seconds to build it, so a worker, a command line tool or a serverless handler can hold a real index without a build.

## Why a specialized engine rather than postgres + pgvector + llama.cpp

That combination is the sensible default and it is the baseline rust-db is measured against: PostgreSQL for storage, the pgvector extension for the vector index, and llama.cpp serving the embedding model over HTTP. It works. Four things about it are structural rather than a matter of tuning, and each one is a place a specialized engine wins.

### Filtering happens after the search, not during it

pgvector evaluates a query's `WHERE` clause after the index scan has already chosen its candidates. A plain HNSW scan produces only `hnsw.ef_search` candidates, so a search restricted to a minority source narrows those candidates down and can be left with almost none. The pgvector answer is `hnsw.iterative_scan`, which makes the scan keep going until enough rows pass the filter. It works, and it costs latency: a filtered search that took a few milliseconds takes tens of milliseconds once the scan has to run long enough to fill the result set.

rust-db applies the filter inside the traversal. A node that fails the filter is still expanded, so the walk can pass through it to reach the region it guards, but it is never admitted to the results. The walk therefore continues until it has collected enough passing chunks, at the cost of a longer walk rather than a repeated scan.

### An exhaustive scan is often the right plan and pgvector will not choose it

When a filter admits 7,000 chunks out of 186,000, comparing the query against all 7,000 is both exactly correct and faster than walking a graph over the whole corpus. rust-db counts what the filter admits, compares that to a measured crossover point, and picks the exhaustive scan when it wins. The result is that narrow filters are the case where accuracy is perfect rather than the case where it collapses.

### The embedding model is a separate process reached over a socket

llama.cpp runs the model in its own program. Every query pays process boundary and HTTP costs before any searching happens, the program has to be started and supervised, and a deployment has two things to keep alive instead of one.

rust-db runs the embedding model in the same process, through the ONNX runtime. Nothing to start, nothing to supervise, no socket. It runs `nomic-embed-text-v1.5`, the same model, at full precision.

### You are paying for durability, transactions, a planner and a wire protocol you are not using

A RAG index is derived data. It is rebuilt from the source documents, so write ahead logging, multiversion concurrency control, a cost based query planner and a network protocol are all cost with no return on this workload. The whole 186,000 chunk corpus below serves from 1.86 GB of memory, which fits on a laptop. That is the fact that makes a purpose built engine worth having: once the index fits in memory, everything PostgreSQL does to survive a power cut is overhead.

## What it does

| Capability | Detail |
|---|---|
| **Meaning based search** | Finds relevant text that shares no words with the query. HNSW graph index, `m = 16`, `ef_construction = 64`. |
| **Word based search** | Finds exact terms, including ticket keys, function names and file paths. Scored with BM25, with prefix expansion through the term dictionary. |
| **Combined ranking** | Merges both result lists. Reciprocal Rank Fusion by default, with a second method available. |
| **Filtering** | By source, workspace, author, date, label, and combinations. Applied during the search rather than afterwards. |
| **Exhaustive search as a real plan** | Chosen automatically when the filter is narrow enough that it is both exactly correct and faster. |
| **Memory compression** | One byte per number instead of four. A quarter of the memory at no measurable accuracy cost. |
| **Save and reopen** | An index is a directory of four files. Reopening takes 5.3 seconds. |
| **Embedding in the same process** | `nomic-embed-text-v1.5` at full precision through the ONNX runtime. No server, no socket. |
| **One dependency** | A Rust library. Nothing to install, no port, no background process. |

## Where these numbers come from

Every measurement below was produced by `rustdb-bench`, a second program in this repository that drives rust-db and a PostgreSQL baseline through one shared interface, so no measurement can be taken of only one of them.

The corpus is **186,781 chunks across 39,366 documents at 768 dimensions**, assembled by this repository from public data: Simple English and English Wikipedia articles and Talk pages from the Wikimedia CirrusSearch dumps, source files from eight open source repositories in eight languages, and real GitHub issue threads from those repositories. Every licence is named in the README. Six sources are represented, in the proportions a real organisation's knowledge tends to take: wiki pages and source files hold three quarters of it, while chat messages, issue threads, design files and boards are each a small minority. That imbalance is the point rather than an accident, because it is what makes filtered search hard.

Correct answers come from three places, none of which needs a person to judge a result:

- **Exhaustive comparison** defines the correct answer for meaning based search. It compares the query against everything, so it cannot be wrong.
- **Direct database queries** define which chunks a filter should admit and which chunks contain a given string.
- **Document identity** defines the correct answer for the whole pipeline. Take a document, use its own title as the query, and count any chunk of that document as correct. People write titles to describe their own content, so a title behaves like a real query. A title shared by two documents is skipped, because then the correct answer would be ambiguous.

Both engines read **byte identical vectors**. Every chunk is embedded once and both engines are loaded with the same numbers, and every query is embedded once and handed to both. That is deliberate: if each engine embedded its own text, a score difference could come from the embedding model rather than from the index, and the comparison would measure nothing.

## Speed

Measured inside the calling process over 120 queries after a warm up, reported as the middle value and the slowest 5%.

| Search type | rust-db | postgres + pgvector | pgvector setting used |
|---|---|---|---|
| Unfiltered, middle | **0.77 ms** | 3.04 ms | iterative scan off |
| Unfiltered, slowest 5%  | **1.76 ms** | 5.45 ms | iterative scan off |
| Filtered to a minority source, middle | **1.65 ms** | 45.06 ms | iterative scan on |
| Filtered to a minority source, slowest 5% | **2.64 ms** | 66.80 ms | iterative scan on |

The last column matters, because the correct pgvector setting is not the same for both rows. Iterative scan belongs on for a filtered search, where without it the result set does not fill, and off for an unfiltered one, where it changes neither the results nor the latency enough to be worth paying for. The baseline is given the right setting for each row rather than one setting for both.

The filtered rows are the ones that decide it, because filtered search is what an application with a search tool per source performs all day. The 45 milliseconds is the cost of restarting the scan until enough rows pass the filter. rust-db reaches the same full result set in 1.65 milliseconds because it never restarts: it filters inside a single traversal.

One honest note on the unfiltered rows. Part of that gap is that rust-db is a library and pays no network cost while PostgreSQL is reached over a connection. That is a real saving in a deployed system, but it is not a claim about index quality, so read it alongside the accuracy figures rather than instead of them.

From a saved index, all three search types over the full corpus:

| Operation | Time |
|---|---|
| Meaning based search, unfiltered | 0.39 ms |
| Word based search | 2.75 ms |
| Combined search | 3.19 ms |
| Meaning based search, filtered to a minority source | 2.17 ms |

## Accuracy

### Finding the right document, whole pipeline

180 queries where the correct answer is known, using a document's own title as the query.

| Measure | rust-db | postgres + pgvector |
|---|---|---|
| Correct document ranked first | **0.878** | 0.867 |
| Correct document in the top ten | **0.978** | 0.950 |
| Overall rank quality | **0.912** | 0.902 |

On 90 harder queries drawn from section headings rather than titles, the gap widens:

| Measure | rust-db | postgres + pgvector |
|---|---|---|
| Correct document ranked first | **0.600** | 0.511 |
| Correct document in the top ten | **0.856** | 0.767 |
| Overall rank quality | **0.722** | 0.650 |

### Filtered search accuracy, per source

Accuracy here is recall at 10 measured against an exhaustive comparison over exactly the chunks the filter admits, which is the definition of the correct answer. Rows returned is out of 50 requested.

| Filtered to | corpus chunks | rust-db plan | rust-db rows | rust-db recall@10 | pgvector rows | pgvector recall@10 |
|---|---|---|---|---|---|---|
| wiki pages | 93,617 | graph | 50 | **0.980** | 50 | 0.952 |
| source files | 47,533 | graph | 50 | **0.924** | 50 | 0.608 |
| chat messages | 17,675 | exhaustive | 50 | **1.000** | 50 | 0.720 |
| issue threads | 11,160 | exhaustive | 50 | **1.000** | 50 | 0.716 |
| design files | 9,149 | exhaustive | 50 | **1.000** | 50 | 0.696 |
| boards | 7,397 | exhaustive | 50 | **1.000** | 50 | 0.852 |

Both engines return all 50 rows here, so the difference is entirely in which rows. The four accuracies of 1.000 are not a rounding artefact: below the crossover rust-db compares the query against every chunk the filter admits, so its answer is the exhaustive answer. pgvector is at its best on the largest source, 0.952 on wiki pages, and loses accuracy as the filter narrows, because a narrower filter makes its scan restart more times before it fills the result set.

This is the table most affected by the baseline settings noted at the end of this page, since raising `hnsw.ef_search` and `hnsw.scan_mem_multiplier` is exactly what raises recall inside a filter. Expect the pgvector column to improve when the corrected run lands.

### Word based search

| Query type | rust-db | postgres full text search |
|---|---|---|
| Natural questions, answer in the top ten | **0.933** | 0.533 |
| Natural questions, rank quality | **0.847** | 0.479 |
| Rare identifiers, answer in the top ten | **0.233** | 0.144 |
| Results returned out of 50 | **50.0** | 12.9 |

Two structural reasons for this gap, both properties of PostgreSQL full text search rather than of a configuration choice. First, `to_tsquery` joins terms with AND, so a chunk has to contain every word of the query. Requiring all of "how does the release process work" matches a small fraction of the chunks that contain `release` or `process`, and long natural questions are exactly the ones most likely to return nothing. Joining the terms with OR instead would return more rows, and that variant has not been measured yet, so treat the AND figures as the default rather than as the ceiling. Second, PostgreSQL ranks with `ts_rank_cd`, which is a coverage density score and not BM25, so it does not account for how rare a term is across the corpus or for how long the chunk is. Chunk lengths here range from a mean of 509 characters on issue threads to 2,222 on design files, so length normalisation is not a detail.

### Trading accuracy against speed

`ef_search` is how wide the graph traversal keeps its candidate list, and it is the one setting a caller turns. Accuracy is against an exhaustive comparison over the whole corpus.

| `ef_search` | Accuracy | Time |
|---|---|---|
| 64 | 0.850 | 0.43 ms |
| 128 (default) | 0.907 | 0.65 ms |
| 256 | 0.938 | 1.17 ms |
| 512 | 0.973 | 2.10 ms |

## Memory and disk

| Measurement | Value |
|---|---|
| Serving an index, peak memory | **1.86 GB** |
| Building an index, peak memory | 2.68 GB |
| Embeddings, uncompressed | 573.9 MB |
| Embeddings, compressed to one byte per number | 144.2 MB |
| Graph of connections | 27.9 MB |
| Chunk text and document attributes | 217.3 MB |
| Total on disk | **819 MB** |

Building peaks higher than serving because a build holds the incoming data and the finished index at once. A process that only answers searches needs the serving figure.

One known inefficiency, stated rather than hidden: the chunk text file is written as readable text and takes 217 MB where the text itself is about 166 MB. A compact encoding would remove roughly 50 MB from disk and shorten the 5 second reopen. It was left alone because it costs nothing at query time.

## Compression

Two independent ways to use less memory. Only one is worth taking.

| Configuration | Bytes per embedding | Accuracy |
|---|---|---|
| Full, uncompressed | 3,072 | 0.995 |
| Full, compressed to one byte per number | 772 | **0.995** |
| Shortened to 512 numbers | 516 | 0.770 |
| Shortened to 256 numbers | 260 | 0.635 |
| Shortened to 64 numbers | 68 | 0.345 |

Compression is free: a quarter of the memory at identical accuracy. Shortening the embedding is expensive on this corpus and is not recommended, which is useful to know because `nomic-embed-text-v1.5` advertises the capability and the cost is not obvious until measured.

## Timings for operating it

| Operation | Time |
|---|---|
| Build an index over 186,829 chunks | 175 s, one processor core |
| Save it to disk | 0.3 s |
| Reopen a saved index | 5.3 s |

## Running the embedding model in the same process

Replacing a separate embedding server with a model running inside the application only holds if the vectors are equivalent, so it was measured rather than assumed.

Over 400 chunks embedded both ways, mean cosine similarity was **0.9860** with none below 0.95. The remaining difference is expected, because llama.cpp was serving the model quantized to Q5_K_M while rust-db runs it at full precision.

That comparison was made against the private corpus this engine was first graded on, and it cannot be repeated here: the corpus this repository builds is embedded in process from the start, so there is no second embedder to compare against. What is checked here is that the stored vectors were made from the stored text, which is a different question and a necessary one, because the corpus text and its vectors are produced in separate steps hours apart.

Similarity is not the number that decides it. Retrieval quality is. Running 120 queries through the same index, once with each set of query vectors:

| Measure | embedding server over HTTP | rust-db in the same process |
|---|---|---|
| Correct answer ranked first | 0.8250 | **0.8250** |
| Correct answer in the top ten | 0.8917 | **0.8917** |
| Overall rank quality | 0.8509 | 0.8487 |

Identical on the first two and within 0.002 on the third. The two disagree on the first result for 12% of queries but are equally often correct, so the disagreement is reshuffling among equally good answers.

These three figures were taken when the engine was first built, against a corpus that is no longer distributed, because they need both embedders running over the same text and this repository no longer ships the server. They are reported as what they are: the measurement that justified removing the server, not something this repository can re-run. What it can check is that the vectors in its embedding cache were produced from the text in that cache, and the `embed-check` subcommand does exactly that.

**One operational requirement.** The ONNX runtime shared library must be present, installed with `brew install onnxruntime` on macOS. It is a single library file, not a process. Nothing needs to be running.

## Correctness

Two checks that pass or fail rather than scoring, because an engine that returns rows it was told to exclude is not a faster engine, it is a wrong one.

**Filtering.** Every returned row was checked against its filter across eleven filter shapes, including a value the corpus does not contain, which has to return nothing rather than everything. 2,500 rows checked. All pass.

**Behavioural guarantees.** The same query returns the same answer every time. No document contributes more than two results. Deleted content never appears. An empty query, a query of only common words, and a 5,000 character query each return nothing rather than failing. A request for zero results returns zero results. All pass.

The engine has **118 tests** of its own and the measurement program has **62**.

## The baseline these comparisons are held to

Beating a badly configured PostgreSQL would prove nothing, so the baseline is a correctly configured one. It runs the same SQL shape against the same schema with the same HNSW parameters, `m = 16` and `ef_construction = 64`, and fuses its two result lists with the same Reciprocal Rank Fusion constants. It reads the same vectors rust-db reads.

Its scan settings are these, each chosen from a measured sweep against an exhaustive comparison rather than by feel:

| setting | filtered search | unfiltered search | why |
|---|---|---|---|
| `hnsw.iterative_scan` | `relaxed_order` | `off` | Without it a filtered search returns almost nothing. On an unfiltered search it changes nothing worth having: recall was identical with it off and on, and latency barely moved, because the only clause left excludes 298 chunks of 186,827. |
| `hnsw.ef_search` | 400 | 100 | A scan cannot return more rows than it collected, so this has to be at least the number of rows requested, and higher raises recall inside a filter. |
| `hnsw.max_scan_tuples` | 40,000 | not applicable | Measured against 200,000, mean recall was 0.788 either way, so the larger value only costs latency. |
| `hnsw.scan_mem_multiplier` | 4 | not applicable | At the default of 1 the iterative scan exhausts its memory budget and stops early, returning as few as 30 rows of 50 and holding mean recall to 0.788. At 4 the short results stop and mean recall reaches 0.856. At 8 nothing changes. |
| ordering | `relaxed_order` over `strict_order` | not applicable | 0.856 against 0.727 mean recall at the same cost, and nothing downstream depends on the within scan ordering because Reciprocal Rank Fusion recomputes it. Those figures come from the private corpus this engine was first graded on, measured against a different database. They are the reason the setting has the value it has, not a result this repository reproduces. |

`hnsw.scan_mem_multiplier` is the one most easily missed, and missing it produces a baseline that looks tuned and is not.

**Status of the figures above.** The comparison numbers on this page come from the first full graded run. That run's baseline had `hnsw.ef_search` at 100 on filtered searches rather than 400, `hnsw.max_scan_tuples` at 200,000, iterative scan left on for unfiltered searches, and `hnsw.scan_mem_multiplier` never set, so it ran at the pgvector default of 1. Every one of those differences makes the baseline weaker than the settings in the table, so the pgvector columns above understate a correctly configured PostgreSQL, most of all on filtered recall. The run against the settings in the table is in progress and this page will carry its numbers. rust-db's own figures, its latency, memory, disk, compression ladder, `ef_search` sweep and correctness gates, do not depend on the baseline and are unaffected.

## What these numbers do not cover

- One corpus and one embedding model. The measurement program works against any corpus, but these figures describe this one.
- Where the correct answer is a document's own title, titles share vocabulary with the text beneath them, which flatters word based search. It flatters both engines equally, so the comparison holds, but the absolute figures are optimistic.
- rust-db holds its index in memory. It suits a corpus that fits in memory, which this one does at under 2 GB. A corpus far larger than available memory needs a different design.
- Only searching was measured. Adding content to an existing index requires rebuilding the graph, which takes the 175 seconds above.
- The PostgreSQL word based search comparison uses AND across query terms, which is what `to_tsquery` does by default. An OR variant would return more rows and has not been measured.
