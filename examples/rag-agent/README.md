# A RAG example your agent can run

This directory holds `greek-philosophy.rdb`, a database of Greek and Roman philosophy. The database
is already built, and every passage in it already has an embedding. Install the embedding model,
start your coding agent in this directory, and ask it a question. You do not download a corpus,
build an index or write code.

RAG (retrieval augmented generation) means the agent searches a database first and writes its answer
from the passages the search returns.

## Terms used on this page

| Term | Meaning |
|---|---|
| embedding | a list of numbers that stands for the meaning of a piece of text. Texts with similar meanings get similar lists |
| cosine distance | how far apart two embeddings are. 0 is the same direction. Smaller means closer in meaning |
| FTS5 | SQLite's full text search table. inillucent reads the same `CREATE VIRTUAL TABLE ... USING fts5` statement |
| BM25 | the formula a full text search uses to rank passages that contain the search words |
| HNSW | an index that finds the nearest embeddings without comparing against every row |

The [glossary](../../docs/glossary.md) explains these and other terms in one sentence each.

## What a run looks like

```
you:    who was Seneca?

agent:  $ inillucent --db greek-philosophy.rdb query \
          "SELECT p.title, p.url, p.body
           FROM passage p, (SELECT embed('search_query: ' || ?1) AS q) AS probe
           ORDER BY vector_distance_cos(p.v, probe.q) LIMIT 5" \
          --params '["who was Seneca"]'

        Seneca the Younger (c. 4 BC - AD 65) was a Stoic philosopher, statesman, and
        dramatist of Ancient Rome. Born in Corduba, he was trained in rhetoric and
        philosophy in Rome, exiled to Corsica in AD 41 under Claudius, returned in 49
        to tutor Nero, and became Nero's advisor when Nero took the throne in 54. His
        influence waned, and in 65 he was executed by forced suicide for alleged
        complicity in the Pisonian conspiracy against Nero.

        Source: Seneca the Younger (https://en.wikipedia.org/wiki/Seneca_the_Younger)
```

This is the first answer a local Qwen model gave through opencode. The agent had only `AGENTS.md` and
the question. The agent put `embed` in a subquery. The shorter query in
[Search by meaning](#by-meaning) returns the same five passages.

```mermaid
flowchart LR
    A["You ask a question"] --> B["The agent reads AGENTS.md"]
    B --> C["The agent runs inillucent query or inillucent search"]
    C --> D["greek-philosophy.rdb returns five passages"]
    D --> E["The agent answers and names the articles"]
```

## Setup

Install the embedding model. The download is about 620 MB the first time.

```sh
inillucent setup-embeddings all
```

`inillucent setup-embeddings all` downloads ONNX Runtime and the `nomic-embed-text-v1.5` model into a
folder for your user. The command checks every file against a digest stored in the build. You do not
set any environment variable afterwards. Check that `embed` answers:

```sh
inillucent query "SELECT length(embed('hello'))"
```

The answer is `3072`: 768 numbers of 4 bytes each.

The published release archives are built with the `embed` feature. A build from a checkout needs the
feature named:

```sh
cargo build --release -p inillucent-cli --features inillucent-cli/embed
```

A build without the `embed` feature answers `embed(TEXT): this build has no embedding support
compiled in` and exits with code 3. A build with the feature and no model installed answers
`embed: no embedding model is installed` and names `inillucent setup-embeddings`.

Then start your agent with this directory as its working directory. `AGENTS.md` tells the agent the
two commands and the rules for answering. `CLAUDE.md` imports `AGENTS.md`.

## The agent at work

Both screenshots show opencode driving a local Qwen model through llama.cpp in this directory. The
agent had no instruction beyond the question. The agent read `AGENTS.md`, ran `inillucent`, and
answered from the rows it got back.

![The agent answering "who was Seneca?": the inillucent query it ran, the JSON it got back, and its answer citing the article](images/agent-who-was-seneca.png)

The corpus answers the next question in pieces. The agent collects the pieces and names the article
each came from: Chrysippus on the soul as fire, Marcus Aurelius on meeting death, and the Stoicism
and Substance theory articles for the general ideas.

![The agent answering "what did the Stoics believe about death?" with four passages, each attributed to its article](images/agent-stoics-on-death.png)

## What is in the database

| | |
|---|---|
| articles | 80 Wikipedia articles on Greek and Roman philosophy |
| passages | 2,661, 1,022 characters each on average |
| embeddings | `nomic-embed-text-v1.5`, 768 dimensions, 32 bit floats |
| indexes | an FTS5 table over the text. There is no vector index. [No HNSW index](#there-is-no-hnsw-index-on-this-table) says why |
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

The articles cover the ancient tradition from Thales to Proclus. They include the Stoics, Epicureans,
Cynics, Sceptics and Neoplatonists, and the concept pages that questions usually ask about.
`corpus/ATTRIBUTION.md` lists every article with a link. `scripts/select-corpus.py` explains why the
articles are chosen from a fixed list of titles instead of by Wikipedia category.

## The two searches

### By meaning

```sh
inillucent --db greek-philosophy.rdb query \
  "SELECT title, url, body FROM passage
   ORDER BY vector_distance_cos(v, embed('search_query: ' || ?1)) LIMIT 5" \
  --params '["what did the Stoics believe about death"]'
```

A search by meaning finds passages about the question even when they share no words with it. "you
cannot step into the same river twice" returns three Heraclitus passages first, and the question
does not contain the word Heraclitus.

**Always add the `search_query: ` prefix.** `nomic-embed-text-v1.5` was trained with
`search_query: ` in front of questions and `search_document: ` in front of stored text. The passages
here were embedded with `search_document: `. A question without the prefix still returns rows, and
the rows are worse matches. No error tells you so.

### There is no HNSW index on this table

[Vector search](../../docs/vector-search.md) describes the HNSW index. This table has none. With
2,661 passages, the query compares the question with every stored embedding, about 8 MB of vectors,
and a search by meaning took 1.2 seconds, including loading the model, when this page was
checked on 24 September 2026.

You can build an HNSW index on this table. `scripts/verify-indexed.sh` does that on a temporary copy
of the database and asks the same ten questions as `scripts/verify.sh` through the index. Each
question has to return the same article. The committed database is not changed.

### By word

```sh
inillucent --db greek-philosophy.rdb search 'Metrodorus' --table passage_fts --k 5
```

Use a keyword search for an exact name or term. A search by meaning is weak on rare names. A question
about `Metrodorus` needs passages that contain that name, and the FTS5 table ranks them with BM25.

## What the distance can and cannot tell you

A nearest neighbor search always returns the number of rows you asked for, however far away they
are. Ask this corpus about Kant and five passages come back. They are the five closest passages in
a database that has no article on Kant.

You can read the distance of the closest passage:

```sh
inillucent --db greek-philosophy.rdb query \
  "SELECT round(min(vector_distance_cos(v, embed('search_query: ' || ?1))), 4) AS nearest
   FROM passage" \
  --params '["who was Seneca"]'
```

These are the distances to the closest passage for eight questions, measured on this database on
24 September 2026:

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

Questions about other subjects score 0.44 or more. The Kant question scores 0.189, between the
questions the corpus answers. The Kant question is about philosophy and so is the corpus, so the
embeddings are close. A large distance means the corpus does not cover the question. A small
distance proves nothing. The only way to catch the Kant case is to read the passages. `AGENTS.md`
tells the agent to do that.

A plain `ORDER BY vector_distance_cos` query gives you only the distance. The `inillucent_search`
table computes a separate confidence value for each result. [Vector search](../../docs/vector-search.md)
describes it.

## Checking the example still works

```sh
scripts/verify.sh
```

`scripts/verify.sh` asks ten questions. Each question names the article its answer has to come from.
The script also checks that every passage has a 768 dimension vector, that a keyword search for
`Metrodorus` finds an Epicurean article, and prints the distances for a question the corpus answers
and questions it does not.

Run `scripts/verify.sh` after an engine change. It catches a database where the corpus was rebuilt
and the vectors were not. Such a database has the right number of rows and valid vectors, and every
vector belongs to the wrong passage.

Set `INILLUCENT` to use a binary that is not on your path:

```sh
INILLUCENT=/path/to/inillucent scripts/verify.sh
```

## Rebuilding the database

The database is committed, so you do not need to rebuild it. Rebuild it after an engine change, or
read the script to see how it was made:

```sh
scripts/build-database.sh
```

| Step | What it does |
|---|---|
| chunk | `scripts/chunk-corpus.py` splits `corpus/greek-philosophy.jsonl` into passages |
| load | `inillucent import` loads the passages |
| embed | one `INSERT ... SELECT` embeds every passage with the `search_document: ` prefix |
| keyword index | `inillucent import` loads the text into the FTS5 table |

`scripts/build-database.sh` uses only the `inillucent` command line and Python. The build of the
committed database took 8 minutes 28 seconds, and about eight of those minutes were the embedding
(recorded in `tasks/task-1907-a-rag-example-an-agent-can-run-tdd.md`).

`scripts/select-corpus.py` chose the articles. The script reads the Wikipedia extracts that
`scripts/fetch-public-corpus.sh` at the top of this repository downloads. The extracts are 1.5 GB and
are not committed, so `scripts/select-corpus.py` records how the list was made and is not part of the
build. `scripts/write-attribution.py` writes `corpus/ATTRIBUTION.md`.

## Licence

The passages are Wikipedia text under
[CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/), and this directory redistributes
them under that licence. `corpus/ATTRIBUTION.md` names every article and links to the page history
that lists its authors. Everything else in this directory is under the repository's own licence.
