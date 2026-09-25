# Inillucent

**A fast embedded database for AI agents.**

Inillucent is an embedded SQL database written in Rust. It speaks SQLite's SQL, and it has vector
search and keyword search built in. Everything lives in one file.

[inillucent.com](https://inillucent.com) &nbsp;·&nbsp;
[Documentation](#documentation) &nbsp;·&nbsp;
[Install](#install) &nbsp;·&nbsp;
[Client libraries](#client-libraries)

## Features

- SQLite's SQL dialect. 402 of 416 probed SQL cases give SQLite's exact answer. A PostgreSQL dialect
  is on the roadmap.
- A storage engine written from scratch in Rust.
- 397% faster than SQLite overall, and 2,885% faster on reads by key.[^1]
- 174% faster than PostgreSQL with pgvector for semantic search.[^3]
- 302% better than PostgreSQL full text search at finding identifiers.[^3]
- A command line and an MCP server with the same commands.
- A write ahead log and crash recovery, tested by cutting the power at every step of a commit.
- Vector columns, HNSW indexes and an embedding model that runs inside your process.
- One database file that several processes can use at the same time.

## Documentation

The full guide is at [inillucent.com/docs](https://inillucent.com/docs). The pages in this
repository are listed in [docs/README.md](docs/README.md). Good places to start:

| Page | What it covers |
|---|---|
| [Product overview](docs/product-overview.md) | what Inillucent is and who it is for |
| [Getting started](docs/getting-started.md) | installing, the four programs, your first database |
| [Glossary](docs/glossary.md) | the database and search terms the pages use |
| [SQL support](docs/sql.md) | what runs, and where it differs from SQLite |
| [Vector search](docs/vector-search.md) | vector columns, HNSW indexes, keyword search and hybrid ranking |
| [Embeddings](docs/embeddings.md) | running the embedding model inside your process |
| [Migrating](docs/migrating.md) | moving in from SQLite, PostgreSQL or MySQL |
| [Performance](docs/performance.md) | speed, processor time and memory against SQLite |
| [Retrieval quality](docs/retrieval-quality.md) | search quality and speed against PostgreSQL with pgvector |
| [AGENTS.md](AGENTS.md) | the starting point for an AI agent that uses or changes this repository |

## Install

**Windows**

```powershell
irm https://inillucent.com/downloads/install.ps1 | iex
```

**macOS and Linux**

```sh
curl -fsSL https://inillucent.com/downloads/install.sh | sh
```

Both scripts check the download against the published `SHA256SUMS`, install into your home
directory, and need no administrator rights.

**Package managers**

| Manager | Command |
|---|---|
| Homebrew | `brew install black-rainbow-labs/inillucent/inillucent` |
| npm | `npm install -g inillucent` |
| pip | `pip install inillucent` |
| Go | `go install github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent-install@latest && inillucent-install` |
| Composer | `composer require black-rainbow-labs/inillucent && vendor/bin/inillucent-install` |
| cargo | `cargo install inillucent-cli inillucent-migrate`. For the `embed()` function, run `cargo install inillucent-cli --features embed` |

A signed macOS installer, `.deb` and `.rpm` packages, and plain archives for every platform are on
[inillucent.com](https://inillucent.com) and the
[GitHub release](https://github.com/Black-Rainbow-Labs/Inillucent/releases/latest). To check a
download by hand:

```sh
minisign -Vm SHA256SUMS -p inillucent.pub     # inillucent.com/downloads/inillucent.pub
sha256sum -c SHA256SUMS --ignore-missing
```

## Your first database

```sh
inillucent create app.rdb
inillucent --db app.rdb exec "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)"
inillucent --db app.rdb exec "INSERT INTO note (body) VALUES (?1)" --params '["hello"]'
inillucent --db app.rdb query "SELECT * FROM note" --output json
```

Every install gives you four programs:

| Program | What it does |
|---|---|
| `inillucent` | the command line, with `--output json` on every command |
| `inillucent-shell` | an interactive shell that works like `sqlite3` |
| `inillucent-mcp` | an MCP server, so an AI agent can use a database with no code written |
| `inillucent-migrate` | builds a database from a legacy retrieval index. `inillucent migrate` copies a SQLite file, a PostgreSQL database or a MySQL database |

## What it does

### Your SQLite, faster

Your SQLite queries, schemas and `sqlite3` scripts run unchanged: joins, recursive CTEs, window
functions, triggers, foreign keys, upserts, `RETURNING`, JSON, FTS5 and more. The storage engine
under that SQL is written from scratch in Rust. It runs 397% faster than SQLite overall and 2,885%
faster on reads by key,[^1] and it uses 49% less processor time.[^2]
[SQL support](docs/sql.md)

### Search by meaning and by keyword

Store embeddings in a `VECTOR(768)` column, index them with HNSW, and order results by
`vector_distance_cos`. Keyword search with BM25 is in the same file. One query can combine the two,
so an agent finds "how does the release process work" and `PROJ-1932` with the same call. Semantic
search is 174% faster than PostgreSQL with pgvector, and finding identifiers is 302% better than
PostgreSQL full text search.[^3]
[Vector search](docs/vector-search.md)

### A search that can come back empty

Most search engines return their ten closest matches even when nothing in the data answers the
question. An agent then writes a confident answer from those matches. Inillucent gives every result a
confidence score, so a search with no good answer returns nothing. On 200 questions with no answer,
PostgreSQL with pgvector returned a result every time. Inillucent returned one for 1 of the 200.
[Retrieval quality](docs/retrieval-quality.md)

### The embedding model runs inside your process

`inillucent setup-embeddings all` downloads the embedding model once. After that, `embed('some text')`
works in any SQL statement. There is no embedding server to deploy or keep running.
[Embeddings](docs/embeddings.md)

### One file, many processes

Tables, vector indexes and keyword indexes all live in one `.rdb` file, and they commit and roll back
together. Several processes can open the file at the same time. One process writes at a time, and
the others wait for up to `PRAGMA busy_timeout`, which is 5 seconds by default.
[Architecture in one page](docs/architecture-overview.md)

### Bring your data with you

`inillucent migrate` copies a SQLite file, a PostgreSQL database or a MySQL database into Inillucent.
It never writes to the source. It checks every table by row count and by checksum, and it
publishes the new file only when every check passes.
[Migrating](docs/migrating.md)

## A first search

[`examples/rag-agent/cli-example/`](examples/rag-agent/cli-example/README.md) holds a ready made
database of Greek philosophy: 80 Wikipedia articles split into 2,661 passages, each with its
embedding. Install the embedding model and ask a question:

```sh
inillucent setup-embeddings all

inillucent --db examples/rag-agent/cli-example/greek-philosophy.rdb query \
  "SELECT title, body FROM passage
   ORDER BY vector_distance_cos(v, embed('search_query: ' || ?1)) LIMIT 5" \
  --params '["who was Seneca"]'
```

[`examples/rag-agent/rust-example/`](examples/rag-agent/rust-example/README.md) serves the same
articles to an agent through an MCP server written in Rust, which chunks, embeds and syncs them
itself. [`examples/`](examples/README.md) describes every example.

## For AI agents

To give an agent a database over MCP:

```json
{
  "mcpServers": {
    "inillucent": {
      "command": "inillucent-mcp",
      "args": ["--db", "app.rdb"]
    }
  }
}
```

Add `--readonly` to refuse every statement that changes data. Add `--root DIR` to refuse any file
outside one directory.

[`agent-skills/`](agent-skills/README.md) has a skill for each common job: installing, querying,
searching, migrating, putting Inillucent in an application, and troubleshooting. Each skill is a
plain `SKILL.md` file. Claude Code loads skills from `~/.claude/skills`, and any other agent can read
them as Markdown.

## Client libraries

From Node, the npm package runs a query and returns rows as objects:

```js
import { query } from 'inillucent';

const rows = await query('SELECT id, body FROM note WHERE id > ?1', { db: 'app.rdb', params: [0] });
```

From Python, the pip package runs the engine inside your process:

```python
from inillucent import Database

with Database("app.rdb") as database:
    rows = database.connect().execute("SELECT id, body FROM note")
```

The Go and PHP packages have the same kind of `query` call. The Node, Go and PHP packages, and the
Python `query` and `run` functions, run the `inillucent` program and read its JSON output. The
Python `Database` class calls the C library directly, which ships in each archive with its header.
[The driver page](drivers/README.md) documents the C library for anyone writing a new binding. Client libraries for TypeScript, Rust, Java and C#
are being built in [inillucent-clients](https://github.com/Black-Rainbow-Labs/inillucent-clients)
and are not on a package registry yet.

## Licence

MIT. See [LICENSE](LICENSE).

[^1]: Measured against SQLite 3.53.4, built from the official source and run as a separate program
    over the same data, with the same SQL, the same durability setting and the same cache size.
    Thirty paired rounds, four runs in a row, at 100,000 rows on Windows x64, on 23 September 2026,
    with both programs on the same eight performance cores. Overall: 397% faster, the weighted
    geometric mean across ten families of work, with a 95% lower bound of 362%. Reads by key:
    2,885% faster. Every result is hashed and compared with SQLite's before its time counts. Six of
    the thirty workloads are slower than SQLite. [Performance](docs/performance.md) names each one.

[^2]: Processor time was 555 ms against 1,082 ms for SQLite for one round of the same plan at
    100,000 rows, on 23 September 2026.

[^3]: Graded on 20 September 2026 against PostgreSQL with pgvector over a corpus of 185,078
    passages at 768 dimensions. Both engines were loaded with the same vectors and given the same
    embedded query. Semantic search: a median of 0.85 ms against 2.32 ms for the faster of two
    pgvector configurations, 174% faster. Keyword search on identifiers: mean reciprocal rank 0.546
    against 0.136 for PostgreSQL full text search, 302% better. Of 17 graded comparisons, Inillucent
    was better on 15, equivalent on 1, inconclusive on 1 and worse on none.
    [Retrieval quality](docs/retrieval-quality.md) has the full table.
