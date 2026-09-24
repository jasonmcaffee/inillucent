# Inillucent

**A highly performant agentic database.**

An embedded database, using a SQL syntax you're familiar with, and the power of vector embeddings for
semantic search. A perfect solution for agentic systems.

[inillucent.com](https://inillucent.com) &nbsp;·&nbsp;
[Documentation](#documentation) &nbsp;·&nbsp;
[Install](#install) &nbsp;·&nbsp;
[Client libraries](#client-libraries)

## What's in the box?

- Complete SQLite dialect implementation. (Upcoming Postgres dialect option)
- Highly performant database written in Rust.
- 400% faster than SQLite overall.[^1]
- 3000% faster on reads by key.[^1]
- 300% faster than Postgres on keyword search.[^3]
- 180% faster than Postgres + pgvector for semantic search.[^3]
- Full featured CLI and MCP toolset.
- Battle tested log and recovery.
- Comprehensive vector embedding support for semantic search.
- Single db file that supports multi process access.

## Documentation

The full guide is at [inillucent.com/docs](https://inillucent.com/docs). The pages in this
repository are indexed in [docs/README.md](docs/README.md). Good places to start:

| | |
|---|---|
| [Product overview](docs/product-overview.md) | what Inillucent is and who it is for |
| [Getting started](docs/getting-started.md) | install, the four programs, your first database |
| [SQL support](docs/sql.md) | what runs, and where it differs from SQLite |
| [Vector search](docs/vector-search.md) | vector columns, HNSW indexes, keyword search and hybrid ranking |
| [Embeddings](docs/embeddings.md) | running the embedding model inside your process |
| [Migrating](docs/migrating.md) | moving in from SQLite, PostgreSQL or MySQL |
| [Performance](docs/performance.md) | every speed, processor and memory figure against SQLite |
| [Retrieval quality](docs/retrieval-quality.md) | search quality and speed against PostgreSQL with pgvector |
| [AGENTS.md](AGENTS.md) | the starting point for an AI agent using or changing this repository |

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

| | |
|---|---|
| Homebrew | `brew install black-rainbow-labs/inillucent/inillucent` |
| npm | `npm install -g inillucent` |
| pip | `pip install inillucent` |
| Go | `go install github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent-install@latest && inillucent-install` |
| Composer | `composer require black-rainbow-labs/inillucent && vendor/bin/inillucent-install` |
| cargo | `cargo install inillucent-cli` |

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

| | |
|---|---|
| `inillucent` | the command line, with `--output json` on every command |
| `inillucent-shell` | an interactive shell that works like `sqlite3` |
| `inillucent-mcp` | an MCP server, so an AI agent can use a database with no code written |
| `inillucent-migrate` | builds a database from a SQLite file, or from a running PostgreSQL or MySQL server |

## What it does

### Your SQLite, only faster

Your SQLite queries, schemas and `sqlite3` scripts run as they are: joins, recursive CTEs, window
functions, triggers, foreign keys, upserts, `RETURNING`, JSON, FTS5 and more. Under that SQL is a
storage engine written from scratch in Rust, which runs 400% faster than SQLite overall and 3000%
faster on reads by key,[^1] using 50% less processor time.[^2]
[SQL support](docs/sql.md)

### Search by meaning and by keyword

Store embeddings in a `VECTOR(768)` column, index them with HNSW, and order results by
`vector_distance_cos`. Keyword search with BM25 sits in the same file and can be combined with vector
search in one query, so an agent can find "how does the release process work" and `PROJ-1932` with
the same call. It is 180% faster than Postgres with pgvector for semantic search, and 300% better at
finding identifiers than Postgres full text search.[^3]
[Vector search](docs/vector-search.md)

### A search that can come back empty

Ask most search engines a question your data can't answer, and they return their ten closest
matches anyway. An agent will write a confident answer from them. Inillucent gives every result a
calibrated confidence score, so a search with no good answer returns nothing. On 200 questions with
no answer, Postgres with pgvector returned a result every time. Inillucent returned one for 0.5% of
them.
[Retrieval quality](docs/retrieval-quality.md#abstention)

### The embedding model runs inside your process

`inillucent setup-embeddings all` downloads the embedding model once, and after that
`embed('some text')` works in any SQL statement. There is no embedding server to deploy or keep
running.
[Embeddings](docs/embeddings.md)

### One file, many processes

Tables, the vector index and the keyword index all live in one `.rdb` file, and they commit and roll
back together. Several processes can open that file at the same time. Writes take turns: one writer
holds the file at a time, and the others wait up to `PRAGMA busy_timeout`.
[Architecture in one page](docs/architecture-overview.md)

### Bring your data with you

`inillucent migrate` copies a SQLite file, a PostgreSQL database or a MySQL database into Inillucent.
It never writes to the source, and it checks every table by row count and by checksum before it
finishes.
[Migrating](docs/migrating.md)

## A first search

[`examples/rag-agent/`](examples/rag-agent/README.md) holds a ready made database of Greek
philosophy: 80 Wikipedia articles split into 2,661 passages, each with its embedding. Install the
embedding model and ask it a question:

```sh
inillucent setup-embeddings all

inillucent --db examples/rag-agent/greek-philosophy.rdb query \
  "SELECT title, body FROM passage
   ORDER BY vector_distance_cos(v, embed('search_query: ' || ?1)) LIMIT 5" \
  --params '["who was Seneca"]'
```

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

Add `--readonly` to refuse every statement that changes data, and `--root DIR` to keep every file
the agent opens inside one directory.

[`agent-skills/`](agent-skills/README.md) has a skill for each common job: installing, querying,
searching, migrating, embedding Inillucent in an application, and troubleshooting. Each is a plain
`SKILL.md` that Claude Code can load from `~/.claude/skills`, and any other agent can read as
Markdown.

## Client libraries

From Node, the npm package runs queries and returns rows as objects:

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

The Go and PHP packages offer the same kind of `query` call. Every language binds to the same C library, which ships in each
archive with its header. [The driver](drivers/README.md) documents it for anyone writing a new
binding. Client libraries for TypeScript, Rust, Java and C# are being built in
[inillucent-clients](https://github.com/Black-Rainbow-Labs/inillucent-clients) and are not on a
package registry yet.

## Licence

MIT. See [LICENSE](LICENSE).

[^1]: Measured against SQLite 3.53.4, built from the official source and run as a separate program
    over the same data, with the same SQL, the same durability setting and the same cache budget.
    Thirty paired rounds, four consecutive runs, at 100,000 rows on Windows x64, on 23 September
    2026, with both programs pinned to the same eight performance cores. Overall: 397% faster, the
    weighted geometric mean across ten families of work, with a 95% lower bound of 362%. Reads by
    key: 2,885% faster. Every result is hashed and compared with SQLite's before its timing counts.
    Six of the thirty individual workloads are slower than SQLite; [Performance](docs/performance.md)
    names each one.

[^2]: Processor time was 555 ms against 1,082 ms for SQLite on the same plan at 100,000 rows, which
    is 50% less.

[^3]: Graded on 20 September 2026 against PostgreSQL with pgvector over a 185,078 passage corpus at
    768 dimensions, with both engines loaded with the same vectors and given the same embedded query.
    Semantic search: a median of 0.85 ms against 2.32 ms for the faster of two pgvector
    configurations, 174% faster. Keyword search on identifiers: mean reciprocal rank 0.546 against
    0.136 for PostgreSQL full text search, 302% better. Across 17 graded measurements, Inillucent
    was better on 15 and worse on none. [Retrieval quality](docs/retrieval-quality.md) has the full
    table.
