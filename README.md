# inillucent

**An embedded database for agents, written in Rust. It runs SQLite's SQL 279% faster than SQLite,
and it does vector and keyword search in the same file — so a local AI agent can query a body of
written material by meaning and by exact term without standing up PostgreSQL, pgvector and an
embedding server.**

[inillucent.com](https://inillucent.com) &nbsp;·&nbsp;
[Documentation](https://inillucent.com/docs) &nbsp;·&nbsp;
[Install](#install) &nbsp;·&nbsp;
[Docs in this repository](docs/README.md)

It is one library and one file. There is no server to start, no port to configure, no connection
string, and no network hop between your application and its index. One `.rdb` file holds ordinary
tables, a full text index and a vector index, and all three commit and roll back together.

|  |  |  |
|---|---|---|
| **279% faster than SQLite 3.53.4** | the same ten workload families at 100,000 rows | [Performance](docs/performance.md) |
| **65% less processor time** | 445 ms against SQLite's 1,266 for the same plan | [Performance](docs/performance.md) |
| **403 of 416 SQL cases byte for byte, none refused** | every case run through both engines and compared byte by byte | [SQL support](docs/sql.md) |
| **Better than pgvector on 15 of 17 graded comparisons, worse on none** | both engines reading identical vectors | [Retrieval quality](docs/retrieval-quality.md) |
| **15% more memory than SQLite** | 42.6 MiB against 37.2 — the one measurement SQLite still wins | [Performance](docs/performance.md#memory) |

[Performance](docs/performance.md) carries all four figures with their intervals, and names the four
workloads that are slower than SQLite along with what each one costs.

---

## Install

```powershell
# Windows
irm https://raw.githubusercontent.com/jasonmcaffee/inillucent/main/packaging/install.ps1 | iex
```

```sh
# macOS and Linux
curl -fsSL https://raw.githubusercontent.com/jasonmcaffee/inillucent/main/packaging/install.sh | sh
```

Or from the package manager the project already uses:

| | |
|---|---|
| **npm** | `npm install -g inillucent`, or `npx inillucent help` with nothing installed |
| **pip** | `pip install inillucent` — the wheel carries the programs and an in process driver |
| **cargo** | `cargo install inillucent-cli` — builds from source, and the fallback on any platform with no prebuilt archive |
| **Homebrew** | `brew install jasonmcaffee/inillucent/inillucent` |
| **Go** | `go install github.com/jasonmcaffee/inillucent/packages/go/cmd/inillucent@latest` |
| **Composer** | `composer require jasonmcaffee/inillucent && vendor/bin/inillucent-install` |

Every one of them installs the same four programs, and every downloader checks the release's
published SHA-256 before it unpacks anything.

## A first database

```sh
inillucent create app.rdb
inillucent --db app.rdb exec "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)"
inillucent --db app.rdb exec "INSERT INTO note (body) VALUES (?1)" --params '["hello"]'
inillucent --db app.rdb query "SELECT * FROM note" --output json
inillucent help
```

Four programs come out of an install or a build:

| | |
|---|---|
| `inillucent` | the command line: 28 verbs, and `--output json` on every one of them |
| `inillucent-shell` | an interactive shell shaped like `sqlite3`, with 63 of its 65 dot commands |
| `inillucent-mcp` | 27 of the same commands served to an AI agent over MCP |
| `inillucent-migrate` | builds a database from a SQLite file, a running PostgreSQL or MySQL server, or a legacy retrieval index |

[Getting started](docs/getting-started.md) covers all four, the exit codes, and the JSON a binding
sees.

## What it does

**SQLite's SQL, on its own storage.** Joins, common table expressions including recursive ones,
window functions, triggers, foreign keys with all five referential actions, `ATTACH`, partial and
expression indexes, `RETURNING`, `ON CONFLICT DO UPDATE`, 212 built in function names, 67 pragmas.
416 cases were run through `inillucent-shell` and through a pinned `sqlite3` 3.53.4 over a fresh
database each, and every byte of both streams compared: **403 produce SQLite's exact bytes, 13 do
not, and none is refused or silently different**. → [SQL support](docs/sql.md)

**Vector search in the same file.** A `VECTOR(N)` column, `vector_distance_cos`, `vector_distance_l2`
and `vector_dot`, `CREATE INDEX ... USING inillucent_hnsw`, and a planner that turns
`ORDER BY vector_distance_cos(v, ?) LIMIT k` into a probe of that index. Recall against an exhaustive
cosine is **1.000**. → [Vector search](docs/vector-search.md)

**Keyword search that finds the identifier a user typed.** BM25 over an inverted index, with
stemming, identifiers kept whole, and weights for coverage, proximity, phrase order and prefix. Fused
with the vector results, so `PROJ-1932` and "how does the release process work" are both answerable
by one query. → [Vector search](docs/vector-search.md#hybrid-retrieval)

**An answer of "nothing here answers that".** Every hit carries a confidence computed on absolute
bounds, separate from the score that ordered the list. Asked questions the corpus does not answer,
PostgreSQL with pgvector returns a confident top result every single time; inillucent does it on
about one question in a hundred. → [Retrieval quality](docs/retrieval-quality.md#abstention)

**The embedding model inside your process.** `nomic-embed-text-v1.5` at full precision through the
ONNX runtime, on the processor or across several GPUs. No embedding server, no socket, no second
thing to keep alive. → [Embeddings](docs/embeddings.md)

**A way in from whatever you already have.** `inillucent migrate` reads a SQLite file, or a running
PostgreSQL or MySQL server over its own wire protocol inside one repeatable read snapshot, so every
table and the schema are as of one instant. The source is never written to, the destination is never
overwritten, and every table is checked by row count and by an order independent digest before
anything is published. → [Migrating](docs/migrating.md)

## For AI agents

[**`AGENTS.md`**](AGENTS.md) is the front door for an agent, and it splits the two audiences: using
inillucent, and changing it.

[**`agent-skills/`**](agent-skills/README.md) is a task shaped page per job, each a plain `SKILL.md`
directory that can be symlinked into `~/.claude/skills` or read as Markdown by anything else:

| skill | when to open it |
|---|---|
| [`inillucent-quickstart`](agent-skills/inillucent-quickstart/SKILL.md) | install it, make a database, run SQL, read the JSON, understand the exit codes |
| [`inillucent-query`](agent-skills/inillucent-query/SKILL.md) | explore and query a database somebody else built |
| [`inillucent-migrate`](agent-skills/inillucent-migrate/SKILL.md) | bring in a SQLite file, a PostgreSQL database or a MySQL database, verified |
| [`inillucent-search`](agent-skills/inillucent-search/SKILL.md) | full text and vector search: `VECTOR(N)`, HNSW, FTS5, hybrid |
| [`inillucent-embed`](agent-skills/inillucent-embed/SKILL.md) | put it in an application, from Rust, Python, Node, Go, PHP or C |
| [`inillucent-mcp`](agent-skills/inillucent-mcp/SKILL.md) | give an agent a database over MCP, safely |
| [`inillucent-troubleshoot`](agent-skills/inillucent-troubleshoot/SKILL.md) | it did something you did not expect |
| [`inillucent-develop`](agent-skills/inillucent-develop/SKILL.md) | change this repository |

To serve a database to an agent over MCP:

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

The 27 MCP tools are generated from the same command table the command line reads, so the two cannot
drift apart. `--readonly` refuses every statement that changes something, decided by the binder
rather than by reading the text. `--root DIR` refuses every path outside one directory.

## In production

A Gmail assistant with 598,560 passages across 64,172 messages and 2,287 attachments ran on
PostgreSQL with pgvector and PostgreSQL full text search. It now runs on inillucent, with no other
database in the process:

| | before | after |
|---|---|---|
| median semantic search | 33.7 ms warm, 80.6 ms cold | **4.41 ms** |
| recall of the correct top 100 | 0.899 | **1.000** |
| real questions the keyword branch answered with nothing | 57% | **0%** |
| index inside the database | 3,167 MB of a 5,849 MB database | deleted |

The same move found six query shapes that go quadratic on this engine and one recovery failure that
is worth reading before a migration. Both are written up in full, including what they cost and how
each was fixed: [Removing PostgreSQL from a 5.8 GB Gmail assistant](docs/real-world-use-cases/nikaya-postgres-to-inillucent.md).

## Documentation

[**`docs/README.md`**](docs/README.md) is the index. The pages, in the order a new reader wants them:

| | |
|---|---|
| [Product overview](docs/product-overview.md) | what inillucent is, who it is for, and the case for it against PostgreSQL with pgvector |
| [Getting started](docs/getting-started.md) | install, the four programs, a first database, the exit codes |
| [SQL support](docs/sql.md) | what runs, what differs from SQLite, and what is refused by name |
| [Vector search](docs/vector-search.md) | `VECTOR(N)` columns, HNSW indexes, `inillucent_search`, hybrid ranking |
| [Embeddings](docs/embeddings.md) | the embedding pipeline, running it on GPUs, comparing models |
| [Migrating](docs/migrating.md) | from a SQLite file, a PostgreSQL server or a MySQL server |
| [Architecture](docs/architecture.md) | how both engines work, in plain terms |
| [Performance](docs/performance.md) | against SQLite: speed, processor time, memory, disk |
| [Feature comparison](docs/feature-comparison.md) | the full 416-case probe, feature by feature |
| [Retrieval quality](docs/retrieval-quality.md) | against pgvector, and how the grading decides a verdict |
| [Where the vectors live](docs/vector-residency.md) | held in memory or read from the file, and what each costs |
| [Roadmap](docs/roadmap.md) | what is not there yet, in the order it is being worked |
| [Repository](docs/repository.md) | the crates, building it, and running the tests |
| [Dependency policy](docs/dependency-policy.md) | what a production crate may link, and why the list is short |
| [Synthetic corpus](tests/synthetic-corpus.md) | building the public corpus every retrieval number is measured on |

## Limits

- **One writer at a time**, and the readers never block it. Several processes can share one file
  under `PRAGMA locking_mode = normal`; the default is `exclusive`, because releasing the file
  between statements costs the speed headline 3.78x to 3.03x. Threads inside one process are not
  supported.
- **The file format is this engine's own.** SQLite files are imported, not opened. A SQLite
  application moves its data across once with `inillucent migrate`.
- **Four workloads are slower than SQLite**: 2,000 updates in one transaction (376% slower),
  building an FTS5 index (150% slower), compiling `SELECT 1` on every call (138% slower), and a
  2,000 row insert batch (72% slower). [Performance](docs/performance.md#the-workloads-that-are-slower)
  says what each one costs and what is being done about it.
- **On Linux the same binary measured 53% faster** rather than 279%. That difference was traced to
  what SQLite pays the operating system on each platform rather than to anything this engine does
  differently there, and the finding is in [Performance](docs/performance.md#linux).
- **Adding content to a retrieval index rebuilds the graph**, on one thread: 132.6 s over 185,078
  passages, and about nine and a half minutes over 598,560.
- **There is no macOS archive yet**, because each platform's archive is built on that platform.

## Building it

```sh
cargo build --release
```

Then [Repository](docs/repository.md) for the crate layout, the test runner, and how every number in
this documentation is reproduced.

## Licence

MIT. See [LICENSE](LICENSE).
