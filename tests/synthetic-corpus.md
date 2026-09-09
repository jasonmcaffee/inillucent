# The synthetic corpus

**Everything needed to build the graded corpus and run the full comparison between inillucent and
PostgreSQL with pgvector, on a machine that has never done it before.** What to install, where the
data comes from, how to assemble the corpus, how to embed it, how to load it into PostgreSQL, how the
grading works, and which results are allowed to differ on another machine.

Nothing here depends on private data or on any other project. Every input is public, so every number
on the score card can be reproduced by anyone with this repository, an internet connection and a few
hours.

Read [Retrieval quality](../docs/retrieval-quality.md) first if you want the results rather than the
recipe.

## Why the corpus is built rather than shipped

inillucent was first graded on a live PostgreSQL database holding one organisation's Confluence pages, Slack messages, JIRA issues, GitHub content, Figma files and Miro boards. That content cannot be published, which meant nobody outside that organisation could reproduce a single number on the score card, and the repository could not be made public at all.

The corpus described here replaces it. The six sources, the documents, the titles, the authors, the spaces, the labels and the identifiers are all constructed by `crates/inillucent-bench/src/synth.rs`. The sentences inside the chunks are real public text, because a word matching index scored against generated filler measures nothing: term frequencies, sentence length, vocabulary growth and the way rare words cluster together are all properties the BM25 scoring formula depends on, and text produced from a template has none of them.

The text is not committed to this repository. It is downloaded and rebuilt on demand. That keeps the repository small, and it satisfies the share alike licences of the Wikipedia material by attribution rather than by redistribution.

## Words used in this document

| Term | What it means |
|---|---|
| **Chunk** | One searchable piece of a document, roughly a paragraph or a section. Searches return chunks. |
| **Document** | One page, message, issue or file, as its source system sees it. A long document becomes many chunks. |
| **Corpus** | The whole body of text being searched. Here, 186,786 chunks across 39,366 documents. |
| **Embedding**, also **vector** | A list of 768 numbers standing for the meaning of a chunk, produced by a trained model. Two chunks on similar topics get similar lists even when they share no words. |
| **pgvector** | An extension for PostgreSQL that lets it store embeddings and search them. It is the baseline inillucent is measured against. |
| **HNSW** | Hierarchical Navigable Small World, a method for finding the nearest embeddings quickly without comparing against every one. Both engines use it. |
| **Exhaustive search** | Comparing the query against every chunk. Always exactly right, and slower when there are many chunks. It is what every accuracy number is measured against. |
| **Recall** | The share of the genuinely correct answers a search returned. If exhaustive search says the best ten chunks are A to J and a search returns eight of them, recall at 10 is 0.8. |
| **Filter** | A restriction on what a search may return, such as only chat messages, or only documents updated since June. |
| **Ground truth** | A set of queries whose correct answers are known without anyone judging results, so a score is a measurement rather than an opinion. |
| **CirrusSearch dump** | A Wikimedia export of the search index rather than the wiki pages. It already contains plain text and a list of section headings, so no wiki markup has to be parsed. |
| **ONNX** | Open Neural Network Exchange, a file format for trained models. The embedding model is run from an ONNX file inside the calling process. |
| **GUC** | Grand Unified Configuration, PostgreSQL's name for a runtime setting such as `hnsw.ef_search`. |

## What the corpus contains

Six sources, each standing for a kind of workplace content, each built from a different public source so the six differ in vocabulary and register the way real sources do.

| Source | Stands for | Built from | Licence |
|---|---|---|---|
| `confluence` | wiki pages | English and Simple English Wikipedia articles | CC BY-SA 4.0 |
| `github` | source files | eight repositories in eight languages | MIT, BSD 3 Clause, Apache 2.0 |
| `slack` | chat threads | Wikipedia Talk and User talk pages | CC BY-SA 4.0 |
| `jira` | issue threads | real GitHub issues from those repositories | factual metadata |
| `figma` | design files | long articles reformatted as frames and text layers | CC BY-SA 4.0 |
| `miro` | boards | articles reformatted as clustered notes | CC BY-SA 4.0 |

The eight repositories are `tokio-rs/tokio` in Rust, `pandas-dev/pandas` in Python, `vuejs/core` in TypeScript, `gohugoio/hugo` in Go, `apache/airflow` in Python, `facebook/react` in JavaScript, `moby/moby` in Go and `symfony/symfony` in PHP. Their licences are MIT, BSD 3 Clause, MIT, Apache 2.0, Apache 2.0, MIT, Apache 2.0 and MIT.

The three sources built from Wikipedia articles draw from one shared pool, and each article is used by exactly one source. This is deliberate. If one article supplied both a wiki page chunk and a design file chunk, a query matching one would also match its twin in another source, and every filtered measurement would be distorted by content that exists twice.

The measured shape, which the builder reproduces:

| Source | Documents | Chunks | Mean characters per chunk |
|---|---|---|---|
| `confluence` | 12,291 | 93,915 | 826 |
| `github` | 5,664 | 47,524 | 596 |
| `slack` | 16,067 | 17,642 | 1,235 |
| `jira` | 1,967 | 11,159 | 500 |
| `figma` | 1,014 | 9,149 | 2,220 |
| `miro` | 2,363 | 7,397 | 1,182 |
| **total** | **39,366** | **186,786** | |

Three properties of that shape matter more than the totals.

**The split between sources decides which retrieval path a filtered query takes.** inillucent chooses between walking the HNSW graph and scanning exactly, and the crossover sits inside the range these six sources span. On this corpus `confluence` and `github` are above it and the other four are below it. Change the sizes and the filtered search scenarios stop measuring what they were written to measure.

**Chunk order correlates with source.** Chunks are numbered in the order they would have been ingested, so the first tenth of the corpus is `confluence` alone and only the last part interleaves all six. A prefix of this corpus is therefore not a sample of it, which is why the harness samples with a stride and why `--limit N` gives nearly one source.

**Titles are written to describe their own content and are unique per document.** One of the three ground truths uses a document's title as a query and counts that document's chunks as correct, skipping any title two documents share. Templated or repeated titles would quietly empty that ground truth rather than fail loudly.

**Every chunk carries its document title and its heading path in its own text.** A chunk begins with the title, then the heading path, joined by ` > `, then the body: `Amphetamine > Uses` and then the article text. This is the property that took longest to get right and it is worth understanding before changing the chunk format.

Two of the graded scenarios query with a title and with a section heading. If the title and the heading are held only as metadata beside the text, neither engine has anything to match, and both score near zero together. Measured: with the heading absent from the body, lexical retrieval by heading scored 0.078 success@10; with the title absent, document identity scored 0.450 success@1. On a corpus carrying both, the same measurements are 0.933 and 0.878. The difference is not difficulty, it is whether the question can be answered at all.

The corpus this one reproduces had both properties for every single chunk: 186,860 of 186,860 contained their document title, and 178,967 of 178,967 with a heading contained that heading. A real ingestion pipeline produces this without trying, because it splits a page at its headings and the breadcrumb lands at the top of each chunk. `synth-check` asserts both at 100%, and the measured mean chunk lengths include the breadcrumb, which is why the body is cut shorter to leave room for it.

## What you need before you start

Hardware. Any machine with 16 GB of memory and 20 GB of free disk will do. The embedding step is the one that cares about the processor, and it cares about how many fast cores there are rather than how many cores are reported. On a machine reporting eighteen cores, of which six were performance cores and twelve were efficiency cores, embedding held between 5 and 7 chunks a second and the whole corpus took about ten hours.

Software, with the versions this was done on:

| Tool | Version used | Why |
|---|---|---|
| Rust | 1.97.1 | builds both crates |
| PostgreSQL | 18.6 | holds the baseline corpus |
| pgvector | 0.8.6 | the baseline vector index. 0.8.0 or later is required, because `hnsw.scan_mem_multiplier` does not exist before it |
| ONNX Runtime | 1.29.0 | runs the embedding model |
| Python | 3.14 | the two extraction scripts |
| `gh` | 2.97.0 | fetches the issue threads, authenticated |
| `git` | any | shallow clones of the eight repositories |

## Step 1: install the tools

```sh
brew install rust postgresql@18 pgvector onnxruntime gh
gh auth login          # the issue fetch needs an authenticated request budget
```

PostgreSQL has to be listening somewhere you know. Everything below assumes port 5433. If yours differs, pass `--database-url` to every command that talks to the database.

The ONNX Runtime library is loaded by name at run time rather than linked, so its path has to be in the environment for every command that embeds anything:

```sh
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib
```

Three commands need that variable set: `synth-embed`, `embed-check` and `grade`. `grade` needs it because it embeds its query sets in process. Forgetting it produces an error naming the variable.

## Step 2: get the embedding model

The model is `nomic-embed-text-v1.5`, in its ONNX export. Five files are needed, in `~/.cache/inillucent-models/nomic-embed-text-v1.5/`:

```
config.json  model.onnx  tokenizer.json  tokenizer_config.json  special_tokens_map.json
```

`model.onnx` is 547 MB. They come from the `nomic-ai/nomic-embed-text-v1.5` repository on Hugging Face. Be aware that a corporate proxy may block Hugging Face: on the machine this was done on, `huggingface.co` returned HTTP 403 through a Zscaler gateway for both the API and file downloads, and the weights had to be fetched from a network without that proxy and copied across. If the download fails with 403 rather than 404, that is what is happening, and it is not a problem with the repository name.

The same directory may also hold `model_quantized.onnx`, an int8 export that runs about three times faster. **Do not use it for the graded corpus.** Measured against the full precision vectors on this corpus, it agrees at a mean cosine of 0.950 with a worst case of 0.935, and not one of 160 sampled chunks agreed above 0.961. That is a larger change to every vector in the corpus than the substitution the engine was originally accepted on, and it would lower absolute retrieval scores for both engines. It is a reasonable way to get a corpus quickly for a smoke test and a bad way to produce a score card.

## Step 3 and Step 4: download the public data and extract it

One script does both:

```sh
cd inillucent
./scripts/fetch-public-corpus.sh
```

It downloads two Simple English Wikipedia CirrusSearch dumps, 635 MB and 329 MB. It shallow clones the eight repositories, about 819 MB. It fetches issue threads with `gh`, bounded to six pages of a hundred per repository, because these repositories hold tens of thousands of issues and the corpus needs about two thousand. Then it runs the two extraction scripts, and it streams the English Wikipedia CirrusSearch dump.

That last part deserves an explanation, because the file is 43 GB. It is never downloaded. The script pipes it through `scripts/extract-wikipedia.py`, which reads compressed records from standard input and stops once it has 60,000 articles of at least 3000 characters, at which point the connection closes. Only the first part of the file is ever transferred. English articles are much longer than Simple English ones and carry a far larger vocabulary, which is why they are worth the trouble: without them the three article based sources cannot reach their chunk counts.

Steps 3 and 4 can be run separately if something fails part way. The extraction scripts are safe to rerun and skip work already done:

```sh
python3 scripts/extract-wikipedia.py ~/.cache/inillucent-corpus/raw ~/.cache/inillucent-corpus/derived
python3 scripts/extract-github.py    ~/.cache/inillucent-corpus/raw ~/.cache/inillucent-corpus/derived
```

What you should have afterwards, in `~/.cache/inillucent-corpus/derived/`:

| File | Records | What it holds |
|---|---|---|
| `enwiki-articles.jsonl` | 60,000 | English Wikipedia articles, plain text with section headings |
| `articles.jsonl` | 81,740 | Simple English Wikipedia articles |
| `talk.jsonl` | 206,339 | Wikipedia discussion pages |
| `code.jsonl` | 18,060 | source files from the eight repositories |
| `issues.jsonl` | 13,110 | GitHub issue threads with a body of 300 characters or more |

Those counts are what a working extraction produces. Being short on `code.jsonl` or `issues.jsonl` is the common failure and the next step will tell you about it.

**A limit on exact reproduction.** Wikimedia keeps only about eleven weeks of CirrusSearch dumps. The dump date used here is 20251222, and by the time you read this it may be gone; set `DUMP_DATE` to a date the server still offers. The GitHub repositories and their issues also move. So a corpus built on another day is not the same text as this one. What is reproducible is the shape: the builder targets the measured document and chunk counts, so a corpus built from a later dump has the same size, the same split between sources, the same chunk length distribution and the same ordering. Absolute retrieval scores may move a little between dumps. The comparison between the two engines is what the grading is for, and both engines read the same corpus whichever dump it came from.

## Step 5: build the corpus

```sh
cargo build --release
./target/release/inillucent-bench synth-build --out ~/.cache/inillucent-corpus/corpus.jsonl
```

This takes a couple of minutes and writes 249 MB. It prints a line per source with the documents and chunks produced against the targets, and it warns if a source ran short of raw material. Document counts should be exact. Chunk counts should be exact or within a few dozen.

`--scale` multiplies every source's document and chunk count while holding the proportions between sources, so `--scale 0.25` builds a quarter sized corpus for a faster cycle and `--scale 2` builds one twice this size. Embedding time scales with it, and it is the embedding that costs hours, so a scale above 1 is a serious commitment.

The assembly is deterministic. It uses a fixed seed, so building twice from the same downloaded material produces the same corpus byte for byte.

## Step 6: check the corpus before embedding it

Do not skip this. Embedding takes hours, and a corpus that cannot supply a ground truth produces a score card with empty scenarios that no one notices.

```sh
./target/release/inillucent-bench synth-check --corpus ~/.cache/inillucent-corpus/corpus.jsonl --per-source 40
```

It runs the three real ground truth generators, the same functions the graded run uses, and refuses the corpus unless every one of them works. It checks that every chunk key is unique, that all six sources supply enough title queries, that natural language heading queries and rare identifier queries exist, that soft deleted documents are present so a filter that forgets to exclude them can be caught, and that the chunk ordering still correlates with source, meaning the first tenth of the corpus covers fewer sources than the whole and the last tenth covers all of them.

It then checks something the above does not: whether those queries can be **answered**. For every generated query it asks whether any chunk counted as a correct answer actually contains the query text, which is the weakest condition under which the question is answerable at all. It also asserts that every chunk carries its own title and its own leaf heading.

This second half exists because its absence cost two complete embedding runs. A corpus can pass every generation check and still be one where the queries have no findable answer, and the symptom is not an error: both engines score near zero together, which reads like a corpus that is merely harder than the last one. Run against the two corpora that had the defect, the check refuses both, reporting 45.8% of document identity queries answerable.

A healthy run prints:

```
are the ground truths answerable
  document identity          240/240 have a correct chunk containing the query text (100.0%)
  natural language headings  120/120 have a correct chunk containing the query text (100.0%)
  rare identifiers           120/120 have a correct chunk containing the query text (100.0%)
  chunks carrying their own title: 186786/186786 (100.0%)
  chunks carrying their own leaf heading: 178814/178814 (100.0%)
```

One wrinkle in reading that output. An identifier query is not a substring of its chunk: the generator strips every character that is not alphanumeric, a dash or an underscore from a whitespace separated word, so `exists_method(return_value)` yields the token `exists_methodreturn_value`, which appears nowhere literally. The check applies the same filtering to the chunk's words rather than searching for a substring. The generator can therefore produce a query that is two identifiers welded together, which no engine can match; it depresses both engines equally and is left as it is, because changing it would make new numbers incomparable with the earlier ones.

A healthy run ends with `the corpus supports every graded scenario`. Anything else names the source and the ground truth that failed.

This check exists because of a real defect it would have caught too late. The code shaped source once contributed zero graded queries, because its titles were file paths and the title ground truth requires at least two words separated by spaces. The score card would have shown five sources where six were expected, and nothing would have failed.

## Step 7: embed the corpus

This is the long step. Budget ten hours, and read the two notes below before starting.

```sh
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib
./target/release/inillucent-bench synth-embed \
  --corpus ~/.cache/inillucent-corpus/corpus.jsonl \
  --cache  ~/.cache/inillucent-corpus/corpus.cache
```

It prints progress every 2000 chunks with a rate and an estimate. It writes each vector to `corpus.vectors` beside the cache as it goes, in corpus order, and assembles `corpus.cache` at the end.

**It is resumable, and that has been tested rather than assumed.** Run the same command again after an interruption and it counts the vectors already written, discards a partial vector at the end of the file if the process died mid write, and continues. This was verified by killing the process part way, appending stray bytes to the vectors file to simulate a torn write, restarting, and then checking every vector in the finished cache against the model.

**Do not rebuild the corpus without re-embedding it.** Position N in the vectors file is chunk N. Rebuilding `corpus.jsonl` and reusing an old vectors file pairs every vector with the wrong chunk. Nothing about that failure looks broken: the index builds, queries return rows, and every retrieval number is quietly wrong. Step 9 is how you find out.

Two things that look like they should make this faster and do not. Raising the ONNX Runtime intra operator thread count makes it slower: 11.3 chunks a second at the default against 9.6 at six threads and 8.0 at eighteen. Running several model sessions in parallel is worse still, 2.0 a second against 6.6 for a single session, while using less total processor time, because each session streams its own 550 MB copy of the weights and the machine runs out of memory bandwidth long before it runs out of cores. Apple's CoreML backend does not work with this model at all: it registers, fails to compile the rotary embedding operators because `attention_mask` has an unbounded dimension, falls back to the processor anyway and runs slower than not using it.

## Step 8: create the database and load it

```sh
createdb -h 127.0.0.1 -p 5433 inillucent_synth
./target/release/inillucent-bench synth-load \
  --corpus ~/.cache/inillucent-corpus/corpus.jsonl \
  --cache  ~/.cache/inillucent-corpus/corpus.cache
```

This creates the extension and the two tables, inserts 39,366 documents and 186,786 chunks with their vectors, then builds nine indexes. Two of them matter to the comparison: a GIN index over `to_tsvector('english', content)` for word matching, and an HNSW index over the embedding column using `vector_cosine_ops` with `m = 16` and `ef_construction = 64`. The schema is the schema the original stack used, reproduced so the baseline SQL runs against it unchanged.

The vectors written here are the same bytes the cache holds. Nothing is recomputed. That is what makes a score difference attributable to the index and the ranking rather than to the embedding model.

`--no-indexes` skips the index building, which is the slow part, and is only useful if you want to inspect the rows. The baseline needs the indexes before grading.

## Step 9: verify the vectors before trusting any score

Two checks, both cheap, both worth running every time.

The first asks whether the vectors in the cache were made from the text in the cache:

```sh
./target/release/inillucent-bench embed-check --cache ~/.cache/inillucent-corpus/corpus.cache --samples 200
```

It re-embeds a sample spread across the whole corpus and compares against what is stored. Because it is the same model over the same text, agreement should be 1.000000 at the minimum, not merely high. It also checks that every vector has the corpus width and is unit length, since cosine distance is computed as a dot product and a vector that is not normalised would score too high. Anything below 0.9995 makes it name the chunks and fail.

The second asks whether PostgreSQL holds the same vectors as the cache. Compare with the pgvector equality operator inside the database:

```sql
SELECT embedding = '[...]'::vector AS same,
       embedding <=> '[...]'::vector AS distance
FROM chunks WHERE id = 1;
```

`same` should be `t` and `distance` exactly `0`.

Do not do this by reading the vector back as text and comparing the numbers yourself. `SELECT embedding` renders the vector as decimal text and loses precision on the way out, which produces differences around 6e-9 that look exactly like a real mismatch and are not. The loader writes through the binary protocol, so nothing is converted on the way in.

## How the testing is conducted

### The two engines, behind one interface

Both engines sit behind one trait in `crates/inillucent-bench/src/engine.rs`, so no scenario can run against only one of them. The pgvector implementation issues the SQL the original stack issued, against the same schema, and fuses the two result lists with the same Reciprocal Rank Fusion constants, so the baseline is that stack's behaviour rather than a fresh approximation of it.

pgvector is graded in two configurations, because reporting only one of them would be misleading in one direction or the other.

`pgvector (extension defaults)` is the extension with nothing set. This is what an installation that never tuned anything actually does, and it is the configuration under which filtered vector search collapses.

`pgvector (correctly configured)` is pgvector configured as well as it can be for this workload. Its settings depend on the query rather than on the engine, because whether an iterative scan helps depends on whether the query carries a filter. A query with a filter gets `hnsw.iterative_scan = relaxed_order`, `hnsw.ef_search = 400` raised to the requested row count when that is larger, `hnsw.max_scan_tuples = 40000` and `hnsw.scan_mem_multiplier = 4`. A query with no filter beyond excluding deleted documents gets the iterative scan off, `hnsw.ef_search = 100`, and the other two settings reset, so a filtered query cannot leave them set on the connection for the next unfiltered one. These live in one pure function, `pg_session_settings`, so a test can assert the exact list of statements without a database.

`hnsw.scan_mem_multiplier` is the setting that changes the numbers most. At the pgvector default of 1 the iterative scan exhausts its memory budget and stops early, so a filtered search returns short result sets even with the iterative scan enabled.

### What makes the comparison fair

Three things, and all three are enforced rather than intended.

Both engines read the same corpus text and the same vectors, because the corpus is embedded once and the identical bytes go to the cache and to the database.

Every query is embedded once and handed to both engines, so the embedding model cancels out of the comparison entirely.

Both engines report hits by the same key, the document identifier and the chunk index joined by `#`. Without a shared key every comparison would silently score zero.

### The three ground truths

Each one is objective, meaning the correct answer is known from the corpus itself and nobody judges results.

**Document titles as queries.** Take a document's title, use it as the query, and count any chunk of that document as correct. Titles here were written by people to describe their own content, so they behave like real queries. Titles shared by two documents are skipped, and so are titles shorter than twelve characters, longer than 160, or fewer than two words. This is the only ground truth that grades the whole pipeline including fusion. Its bias is stated rather than hidden: a title shares vocabulary with its own body, which flatters word matching. It flatters both engines equally.

**Section headings as queries.** Headings read like questions somebody would type rather than like titles. Only headings of at least fifteen characters and three words that appear in at most four chunks are used, because a heading appearing everywhere measures nothing.

**Rare identifiers as queries.** Tokens drawn from the corpus that look like a ticket key, a function name or a versioned name rather than an English word, restricted to those appearing in at most five chunks. Real source code supplies most of them, and ticket keys are threaded through the prose sources at a low rate the way a real document references a ticket.

### The scenario families

The graded run covers approximation accuracy against exhaustive cosine with no filter, filtered vector search for each of the six sources, filter correctness as a pass or fail gate, word matching for both natural language and identifiers, the whole pipeline through fusion, the two fusion methods against each other, quantization and the Matryoshka width ladder, and latency.

Two of them are gates rather than scores. Filter correctness checks that every returned chunk actually satisfies the predicate and that no search returns more rows than the predicate admits, including a filter naming a source the corpus does not contain, which must select nothing rather than everything. These pass or fail; they are not graded on a curve.

## Step 10: run the graded suite

```sh
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib
./target/release/inillucent-bench grade \
  --cache ~/.cache/inillucent-corpus/corpus.cache \
  --per-source 30
```

Twenty minutes or so, most of it index builds: one main index plus eleven smaller ones for the quantization ladder. It writes `inillucent-scorecard.md` into the working directory and the same measurements as JSON beside it, so the card can be re-rendered or re-judged without paying for the run again. Run it from the repository root, or pass `--out` with the path you want.

The default used to be `../inillucent-scorecard.md`, which from the repository root wrote the card to the parent directory, outside the repository entirely. That was found while checking this document against the code and is now fixed, but a card sitting one directory above the repository is what an older build produced.

`--inillucent-only` skips both pgvector configurations, which is the fast loop when changing inillucent alone. `--per-source` sets how many title queries each source contributes; 30 or 40 are the usual values, and a larger number reduces the noise in every per source figure at the cost of time.

The unit tests are separate and take under a minute:

```sh
cargo test --release
```

That is 255 tests across the two crates. They should all pass with no warnings.

## Reading the score card

The card opens with a verdict over the **primary** measurements only. Each family declares one metric that is judged and the rest are diagnostics: they are printed and they do not vote, because nDCG, success@1, success@10 and reciprocal rank are four views of one ranking and counting each separately turns one result into four.

Each primary comparison is decided against the better of the two pgvector configurations by a 95% paired bootstrap interval and a paired randomization test over the per-query scores, against a practical threshold declared before the run: 0.01 on the ranking measures, five per cent on latency. The verdict is *better* when the interval clears both zero and the threshold, *equivalent* when the whole interval sits inside it, *worse* in the other direction, and *inconclusive* when the run cannot tell. Anything worse is listed explicitly with its numbers, and a run that cannot separate two engines says so rather than rounding up.

Beneath the headline, every primary comparison is printed with its delta, its interval, its p-value, the number of queries behind it and how many of those queries the two engines answered differently. A comparison resting on three queries is worth reading with suspicion however small its p-value, and that column is there so it can be.

The provenance table names the run directory. `runs/<id>/per-query.jsonl` holds one line per engine per query — the ranking, each hit’s relevance grade, the component scores, the latency and the metrics that query contributed — so a miss can be looked at rather than guessed at, and a comparison can be recomputed or re-judged without paying for the run again.

Read the filtered vector search table first. It is where the difference between the engines is largest and least ambiguous, and it reports for each source how many chunks the filter admits, which retrieval path inillucent chose, how many rows of the fifty requested came back, and recall within the filter.

## What may differ on another machine, and what must not

Latency will differ, and it is the measurement most sensitive to everything else happening on the machine. Do not compare latency figures taken while something else was running. Two graded runs taken on this machine while an embedding job was using around 670 per cent of the processor produced latencies inflated enough to be useless, while their recall figures were unaffected.

Absolute retrieval scores may differ between corpora built from different dump dates, because the text differs. Retrieval difficulty is a property of the content as well as of the engine.

What must not differ is the relationship between the engines, and specifically these: filtered vector search must still collapse for pgvector at the extension defaults and still work for inillucent; both correctness gates must pass; and inillucent's approximation of exhaustive cosine with no filter must stay high.

If a number moves, explain why rather than adjusting settings until it matches.

## Costs, measured

| Step | Time | Disk |
|---|---|---|
| download and extract | 20 to 40 minutes, mostly the eight clones | 1.9 GB raw, 2.3 GB extracted |
| build the corpus | 2 minutes | 249 MB |
| check the corpus | under a minute | none |
| embed the corpus | about 10 hours at 5 to 7 chunks a second | 574 MB of vectors, 800 MB cache |
| load PostgreSQL and build the indexes | 15 to 25 minutes | about 2 GB in the database |
| the graded suite | about 20 minutes | score card, and 819 MB if an index is saved |
| the unit tests | under a minute | none |

The embedding model needs 654 MB on disk. Peak memory is about 2.7 GB while building an index and 1.86 GB for a process that only answers queries.

## Traps

**`ORT_DYLIB_PATH` unset.** Affects `synth-embed`, `embed-check` and `grade`. The error names the variable.

**pgvector older than 0.8.0.** `hnsw.scan_mem_multiplier` does not exist, so the correctly configured baseline silently is not.

**Querying `pg_settings` for the `hnsw.*` settings on a fresh connection.** It returns nothing, because the extension's settings are only registered once its library is loaded into that session. Read them back with `current_setting` after setting them.

**`--limit N` as a way to get a smaller corpus.** It takes a prefix, and a prefix of this corpus is nearly all one source. Use `--scale` on `synth-build` instead, which shrinks every source together.

**Comparing vectors by reading them back as text.** Described in Step 9. It produces a mismatch of about 6e-9 that is an artefact of decimal rendering.

**Assuming a reported core count is usable parallelism.** On the machine this was done on, eighteen cores were six performance cores and twelve efficiency cores, which is most of the difference between the throughput first estimated and the throughput actually achieved.

## Cleaning up

The downloaded material, the corpus, the vectors, the cache and any saved index are all rebuildable and safe to remove. They live in `~/.cache/inillucent-corpus/` and wherever the cache was written. `.gitignore` already excludes the corpus, the vectors, the caches and any saved index, so none of them can be committed by accident.

The database can be dropped and recreated from the cache with Step 8 alone, without re-embedding anything, as long as `corpus.jsonl` and `corpus.cache` are still the pair that were made together.
