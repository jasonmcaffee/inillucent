# Ask an agent about Greek philosophy, with the command line

This folder holds `greek-philosophy.rdb`, a database of 80 Wikipedia articles on Greek and Roman
philosophy. Every passage in it already has an embedding. You install inillucent and its embedding
model, start a coding agent in this folder, and ask a question. The agent searches the database with
the `inillucent` command line and answers from the passages it finds.

You do not download a corpus, build an index or write code. [`AGENTS.md`](AGENTS.md) tells the agent
which two commands to run and how to answer.

RAG (retrieval augmented generation) means the agent searches first and writes its answer from what
the search returned. [`../rust-example/`](../rust-example/README.md) does the same job with an MCP
server written in Rust, which builds and updates its own database.

## Terms used on this page

| Term | Meaning |
|---|---|
| embedding | a list of numbers that stands for the meaning of a text. Texts with similar meanings get similar lists |
| cosine distance | how far apart two embeddings point. 0 is the same direction. Smaller means closer in meaning |
| FTS5 | SQLite's full text search table. inillucent reads the same `CREATE VIRTUAL TABLE ... USING fts5` statement |
| BM25 | the formula a full text search uses to rank the passages that contain the search words |
| HNSW | an index that finds the nearest embeddings without comparing the question with every row |

[The glossary](../../../docs/glossary.md) explains these and other terms in one sentence each.

## 1. Install inillucent

Use whichever installer you already have. Each one installs the `inillucent` command line.

| Source | Command |
|---|---|
| Windows | `irm https://inillucent.com/downloads/install.ps1 \| iex` |
| macOS and Linux | `curl -fsSL https://inillucent.com/downloads/install.sh \| sh` |
| Homebrew | `brew install black-rainbow-labs/inillucent/inillucent` |
| npm | `npm install -g inillucent` |
| pip | `pip install inillucent` |
| cargo | `cargo install inillucent-cli --features embed` |

Then install the embedding model. The download is about 620 MB the first time.

```sh
inillucent setup-embeddings all
```

`inillucent setup-embeddings all` downloads ONNX Runtime and the `nomic-embed-text-v1.5` model into a
folder in your user profile, and checks every file against a digest stored in inillucent. Check that
the `embed()` function answers:

```sh
inillucent query "SELECT length(embed('hello'))"
```

The answer is `3072`: 768 numbers of 4 bytes each.

## 2. Ask a question

Start your agent with this folder as its working directory. Claude Code reads `CLAUDE.md`, which
points at `AGENTS.md`. opencode, Codex and most other agents read `AGENTS.md` directly.

```sh
cd examples/rag-agent/cli-example
claude            # or opencode, codex, ...
```

Then ask it something:

```
you:    who was Seneca?

agent:  $ inillucent --db greek-philosophy.rdb query \
          "SELECT title, url, body FROM passage
           ORDER BY vector_distance_cos(v, embed('search_query: ' || ?1)) LIMIT 5" \
          --params '["who was Seneca"]' --output json

        Seneca the Younger (c. 4 BC to AD 65) was a Stoic philosopher, statesman and
        dramatist of Ancient Rome. He was born in Corduba, trained in rhetoric and
        philosophy in Rome, exiled to Corsica in AD 41, and recalled in 49 to tutor
        Nero. In 65 he was forced to take his own life for his alleged part in the
        Pisonian conspiracy against Nero.

        Source: Seneca the Younger (https://en.wikipedia.org/wiki/Seneca_the_Younger)
```

```mermaid
flowchart LR
    A["You ask a question"] --> B["The agent reads AGENTS.md"]
    B --> C["The agent runs inillucent query or inillucent search"]
    C --> D["greek-philosophy.rdb returns five passages"]
    D --> E["The agent answers and names the articles"]
```

Both screenshots below show opencode driving a local Qwen model through llama.cpp in this folder. The
agent had no instruction beyond the question and `AGENTS.md`.

![The agent answering "who was Seneca?": the inillucent query it ran, the JSON it got back, and its answer citing the article](images/agent-who-was-seneca.png)

The corpus answers the next question in pieces. The agent collects the pieces and names the article
each one came from: Chrysippus on the soul as fire, Marcus Aurelius on meeting death, and the
Stoicism and Substance theory articles for the general ideas.

![The agent answering "what did the Stoics believe about death?" with four passages, each attributed to its article](images/agent-stoics-on-death.png)

## What is in the database

| | |
|---|---|
| articles | 80 Wikipedia articles on Greek and Roman philosophy, from Thales to Proclus |
| passages | 2,661, of 1,022 characters on average |
| embeddings | `nomic-embed-text-v1.5`, 768 numbers per passage, 32 bit floats |
| indexes | an FTS5 table over the text, and no vector index. [No HNSW index](#there-is-no-hnsw-index) says why |
| size | 21 MB, committed to the repository |

```sql
CREATE TABLE passage (
  id    INTEGER PRIMARY KEY,
  title TEXT NOT NULL,   -- the Wikipedia article
  url   TEXT NOT NULL,   -- where it came from
  body  TEXT NOT NULL,   -- "Plato > the passage"
  v     VECTOR(768)
);
CREATE VIRTUAL TABLE passage_fts USING fts5(id, title, body);
```

The articles are in [`../corpus/greek-philosophy.jsonl`](../corpus/greek-philosophy.jsonl), which
the Rust example reads too. [`../corpus/ATTRIBUTION.md`](../corpus/ATTRIBUTION.md) lists every
article with a link.

## The two searches

### Search by meaning

```sh
inillucent --db greek-philosophy.rdb query \
  "SELECT title, url, body FROM passage
   ORDER BY vector_distance_cos(v, embed('search_query: ' || ?1)) LIMIT 5" \
  --params '["what did the Stoics believe about death"]'
```

A search by meaning finds passages about the question even when they share no words with it. "you
cannot step into the same river twice" returns three Heraclitus passages first, and the question
does not contain the word Heraclitus.

**Keep the `search_query: ` prefix.** `nomic-embed-text-v1.5` was trained with `search_query: ` in
front of questions and `search_document: ` in front of stored text, and the passages here were
embedded with `search_document: `. A question without the prefix still returns rows. The rows are
worse matches, and no error says so.

### Search by word

```sh
inillucent --db greek-philosophy.rdb search 'Metrodorus' --table passage_fts --k 5
```

Use a keyword search for an exact name or term. A search by meaning is weak on rare names. A question
about `Metrodorus` needs the passages that contain that name, and the FTS5 table ranks them with
BM25.

### There is no HNSW index

[Vector search](../../../docs/vector-search.md) describes the HNSW index. This table has none. With
2,661 passages the query compares the question with every stored embedding, about 8 MB of vectors.
A search by meaning took 1.2 seconds on 24 September 2026, and most of that was loading the model.

`scripts/verify-indexed.sh` builds an HNSW index on a temporary copy of the database and asks ten
questions through it. The committed database is not changed.

## What the distance can and cannot tell you

A nearest neighbor search always returns the number of rows you asked for, however far away they
are. Ask this corpus about Kant and five passages come back. They are the five closest passages in a
database that has no article on Kant.

```sh
inillucent --db greek-philosophy.rdb query \
  "SELECT round(min(vector_distance_cos(v, embed('search_query: ' || ?1))), 4) AS nearest
   FROM passage" \
  --params '["who was Seneca"]'
```

The distance to the closest passage for eight questions, measured on this database on 24 September
2026:

| question | nearest |
|---|---:|
| the allegory of the cave | 0.153 |
| who was Seneca | 0.186 |
| man is the measure of all things | 0.203 |
| what did the Stoics believe about death | 0.239 |
| **what did Kant think about the categories of understanding** | **0.189** |
| the best recipe for sourdough bread | 0.441 |
| who won the 1994 World Cup | 0.500 |
| how do I configure a Kubernetes ingress controller | 0.533 |

Questions about other subjects score 0.44 or more. The Kant question scores 0.189, among the
questions the corpus answers, because the Kant question and the corpus are both about philosophy. A
large distance means the corpus does not cover the question. A small distance proves nothing. The
only way to catch the Kant case is to read the passages, and `AGENTS.md` tells the agent to do that.

The Rust example stores its passages in an `inillucent_search` table as well, which gives each
result a `confidence` from 0 to 1. [Its README](../rust-example/README.md#vector-column-or-inillucent_search-table)
compares the two.

## Check that the example still works

```sh
scripts/verify.sh
```

`scripts/verify.sh` asks ten questions. Each question names the article its answer has to come from.
The script also checks that every passage has a 768 number vector, that a keyword search for
`Metrodorus` finds an Epicurean article, and prints the distances for one question the corpus
answers and two it does not.

`scripts/verify.sh` catches a database whose corpus was rebuilt and whose vectors were not. Such a
database has the right number of rows and valid vectors, and every vector sits next to the wrong
passage.

Set `INILLUCENT` to use a binary that is not on your path:

```sh
INILLUCENT=/path/to/inillucent scripts/verify.sh
```

## Rebuild the database

The database is committed, so you do not need to rebuild it. `scripts/build-database.sh` shows how it
was made:

```sh
scripts/build-database.sh
```

| Step | What it does |
|---|---|
| chunk | `scripts/chunk-corpus.py` splits `../corpus/greek-philosophy.jsonl` into passages in `build/` |
| load | `inillucent import` loads the passages |
| embed | one `INSERT ... SELECT` embeds every passage with the `search_document: ` prefix |
| keyword index | `inillucent import` loads the text into the FTS5 table |

`scripts/build-database.sh` uses only the `inillucent` command line and Python. The build of the
committed database took 8 minutes 28 seconds, and about eight of those minutes were the embedding.

`scripts/select-corpus.py` records how the 80 articles were chosen from a Wikipedia dump. It is not
part of the build. `scripts/write-attribution.py` writes `../corpus/ATTRIBUTION.md`.

## Licence

The passages are Wikipedia text under
[CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/), and this folder redistributes them
under that licence. `../corpus/ATTRIBUTION.md` names every article and links to the page history that
lists its authors. Everything else in this folder is under the repository's own licence.
