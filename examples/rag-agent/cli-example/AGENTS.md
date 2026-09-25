# AGENTS.md: answering questions from `greek-philosophy.rdb`

This directory holds a small retrieval database. When you are asked a question about Greek or Roman
philosophy, **search `greek-philosophy.rdb` and answer from the passages it returns**. Do not answer
from memory. Do not write code. The two commands below are all you need.

`greek-philosophy.rdb` holds 80 Wikipedia articles split into 2,661 passages. Each passage has a 768
dimension embedding made by `nomic-embed-text-v1.5`. The database is already built. You do not index,
embed or load anything first.

**Use the `inillucent` command line, even when you also have an `inillucent` MCP server.** The MCP
server may be set up for another database, and it may be an older build without the `embed`
feature. Without `embed`, only the keyword search works. Keyword search still returns passages, so
the missing function looks like a thin corpus. An MCP server that holds `greek-philosophy.rdb` open
can also lock the file against the command line. If either happens, tell the user. Do not fall back
to keyword search and present the result as an answer.

## Search by meaning

Use this search by default. It finds passages about the question even when they share no words with
the question.

```sh
inillucent --db greek-philosophy.rdb query \
  "SELECT title, url, body FROM passage
   ORDER BY vector_distance_cos(v, embed('search_query: ' || ?1)) LIMIT 5" \
  --params '["who was Seneca"]' --output json
```

**Copy the statement as written. Change only the question in `--params`.** Two parts of it matter:

| Part | Why |
|---|---|
| `'search_query: ' \|\| ?1` | The model was trained with `search_query: ` in front of questions. The passages were stored with `search_document: `. Without the prefix the search still returns rows, and the rows are worse matches. Always keep the prefix |
| `--params` | Pass the question as a parameter. A question with an apostrophe, such as "what is Plato's cave", breaks the statement when pasted into the SQL |

## Search by word

Use this search when the question depends on an exact name or term, such as `Metrodorus`,
`ataraxia` or `Lyceum`. A search by meaning is weak on rare names, and a keyword search is good at
them.

```sh
inillucent --db greek-philosophy.rdb search 'Metrodorus' --table passage_fts --k 5 --output json
```

The query uses FTS5 syntax: bare words must all appear, `"a phrase"` goes in double quotes, and `OR`
and `NOT` work.

Each row has `id`, `title` and `body`. The rows have no `url`. The `id` is the `passage.id` of the
row, so `SELECT url FROM passage WHERE id = ?1` gives you a URL to cite.

**When a question has a meaning and a rare word, run both searches.** Use the passages that answer
the question.

## Answering

- **Cite what you used.** Every row from `passage` has a `title` and a `url`. Name the articles your
  answer came from.
- **Say when the corpus does not cover the question. Do not rely on the distance to tell you.** A
  nearest neighbor search always returns the number of rows you asked for, however far away they
  are. "what did Kant think about the categories of understanding" returns five passages, just as
  "who was Seneca" does, and the corpus has no article on Kant.

  You can add `vector_distance_cos(v, embed('search_query: ' || ?1)) AS distance` to the `SELECT`
  list. The distance catches only one of the two cases. Measured on this corpus:

  | Question | Distance to the closest passage |
  |---|---|
  | questions the corpus answers | 0.15 to 0.24 |
  | questions about other subjects: Kubernetes, sourdough, the 1994 World Cup | 0.44 to 0.53 |
  | the Kant question | 0.19 |

  The Kant question is about philosophy, and so is the corpus, so its distance falls among the
  questions the corpus answers. A large distance means the corpus does not cover the question. A
  small distance proves nothing.

  **Read the passages.** If the passages do not answer the question, say so. Do not build a
  paragraph out of passages that do not answer it.
- **Each passage body starts with its article title**, as `Plato > ...`. The title is there on
  purpose. It lets a search for "Plato" find a passage whose sentences only say "he".

## If a command fails

| The command says | What to do |
|---|---|
| `embed(TEXT): this build has no embedding support compiled in` (exit code 3) | This `inillucent` was built without the `embed` feature. Tell the user. A published release archive has the feature. A build from a checkout needs `--features inillucent-cli/embed` |
| `embed: no embedding model is installed` | Run `inillucent setup-embeddings all`. The download is about 620 MB |
| `database is locked` | Another program has the file open, most likely an MCP server. Tell the user. Do not switch to keyword search and present the result as the answer |
| `there is no database at ...` | Run the command from this directory, or give `--db` the full path |
| exit code 3, status `unsupported` | The engine has not built that SQL feature. The SQL is not wrong, and rewording it will not help |

## What else is here

| File | What it is |
|---|---|
| `README.md` | the same example written for a person, with the setup step and what to expect |
| `../corpus/greek-philosophy.jsonl` | the 80 articles the database was built from, one per line |
| `../corpus/ATTRIBUTION.md` | where each article came from, and its licence |
| `scripts/build-database.sh` | rebuilds the database from the corpus with the `inillucent` command line |
| `scripts/verify.sh` | asks ten questions and checks the article each one has to find. Run it after an engine change |
| `scripts/verify-indexed.sh` | asks the same ten questions through an HNSW index built on a temporary copy |
