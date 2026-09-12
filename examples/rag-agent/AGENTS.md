# AGENTS.md — answering questions from `greek-philosophy.rdb`

This directory holds a small retrieval database and nothing else. When you are asked a question
about Greek or Roman philosophy, **search this database and answer from what it returns**. Do not
answer from memory, and do not write any code — the two commands below are the whole interface.

`greek-philosophy.rdb` holds 80 Wikipedia articles, split into passages, each with a 768 dimension
embedding produced by `nomic-embed-text-v1.5`. It is already built. Nothing has to be indexed,
embedded or loaded first.

**Use the `inillucent` command line for this, even if you already have an `inillucent` MCP server.**
That server was configured for some other database and may be an older build; a build without the
embedding feature answers `no such function: embed`, and the only thing left working is keyword
search — which returns passages, so the failure looks like a thin corpus rather than a missing
function. A server holding this file open will also lock it against the command line. If that
happens, say so rather than falling back to keyword search and calling it an answer.

## Search by meaning

This is the one to reach for by default. It finds passages that are *about* the question even when
they share no words with it.

```sh
inillucent --db greek-philosophy.rdb query \
  "SELECT title, url, body FROM passage
   ORDER BY vector_distance_cos(v, embed('search_query: ' || ?1)) LIMIT 5" \
  --params '["who was Seneca"]' --output json
```

Two parts of that are not optional. **Copy the statement as it is written**, changing only the
question in `--params`.

- **`'search_query: ' || ?1`.** The model was trained with `search_query: ` on questions and the
  passages here were stored with `search_document: `. Leaving the prefix off still returns rows, and
  they are quietly worse. Never leave it off.
- **`--params`, not the question pasted into the SQL.** A question with an apostrophe in it —
  "what is Plato's cave" — breaks the statement otherwise.

## Search by word

Use this when the question turns on an exact name or term rather than a meaning — `Metrodorus`,
`ataraxia`, `Lyceum`. Semantic search is poor at rare tokens and this is good at them.

```sh
inillucent --db greek-philosophy.rdb search 'Metrodorus' --table passage_fts --k 5 --output json
```

The query syntax is FTS5's: bare words are ANDed, `"a phrase"` is quoted, and `OR` and `NOT` are
available.

The rows come back as `id`, `title` and `body`. There is no `url` on them — `id` is the `passage.id`
the row came from, so `SELECT url FROM passage WHERE id = ?1` gets you one to cite.

**When a question has both a meaning and a rare word in it, run both and use whichever returned
passages that actually answer it.**

## Answering

- **Cite what you used.** Every row carries a `title` and a `url`. Name the articles your answer came
  from.
- **Say when the corpus does not cover it, and do not trust the distance to tell you.** A
  nearest-neighbour search always returns the number of rows you asked for, however far away they
  are. Five passages come back for "what did Kant think about the categories of understanding" as
  readily as for "who was Seneca", and there is no Kant in this corpus at all.

  Adding `vector_distance_cos(v, embed('search_query: ' || ?1)) AS distance` to the projection
  gives you a number, and
  the number only catches one of the two cases. Measured on this corpus: questions it answers sit at
  **0.15 to 0.24**; questions from another subject entirely — Kubernetes, sourdough, the 1994 World
  Cup — sit at **0.44 to 0.53**; and the Kant question sits at **0.19**, right in the answerable
  band, because it is a philosophy question and this is a philosophy corpus. So a large distance is
  worth acting on and a small one proves nothing.

  **Read the passages.** If they do not answer the question, say so instead of assembling a paragraph
  out of them.
- **The passage text begins with its article title**, as `Plato > …`. That is deliberate, not a
  formatting bug: it is what lets a search for "Plato" reach a passage whose sentences all say "he".

## If a command fails

| what it says | what to do |
|---|---|
| `no such function: embed` | this build has no embedding model in it. `inillucent setup-embeddings all` installs one; if it still says this, the binary was built without `--features embed` |
| `database is locked` | something else has the file open — most likely an MCP server. Close it; do not switch to keyword search and present that as the answer |
| `no such file` on the database | run the command from this directory, or give `--db` the full path |
| exit code 3, status `unsupported` | the engine has not built that construct. It is not a syntax error and rewording will not help |

## What else is here

- `README.md` — the same thing written for a person, with the setup step and what to expect.
- `corpus/greek-philosophy.jsonl` — the articles the database was built from, one per line.
- `corpus/ATTRIBUTION.md` — where each article came from, and its licence.
- `scripts/build-database.sh` — rebuilds the database from the corpus, using only this command line.
- `scripts/verify.sh` — ten questions with the article each one has to find. Run it after an engine
  change.
