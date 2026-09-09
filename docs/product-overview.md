# Product overview

**What inillucent is, who it is for, and the case for it.** If you want to install it and run a
query, go to [Getting started](getting-started.md) instead.

## What it is

An embedded database written in Rust, in one library and one file.

It speaks SQLite's SQL on its own storage, and it holds a vector index and a keyword index in the
same file as your tables. One `.rdb` can carry ordinary rows, an HNSW graph over an embedding column
and a BM25 index over your text, and all three commit and roll back together.

There is no server to start, no port to configure, no connection string, and no network hop between
your application and its index. Four programs come out of an install: a command line, a shell shaped
like `sqlite3`, an MCP server for AI agents, and a migrator that reads a SQLite file or a running
PostgreSQL or MySQL server.

## Who it is for

**Somebody running a local AI agent.** The agent needs to query a body of written material — a
mailbox, a wiki, a repository, a set of tickets — by meaning and by exact term, and needs it fast
enough to do several searches inside one answer. Today that means PostgreSQL, the pgvector extension,
and an embedding model served over a socket. inillucent replaces all three with a library.

**Somebody who already uses SQLite and wants it faster.** The SQL is the same, 403 of 416 probed
cases produce SQLite's exact bytes, and nothing is refused that SQLite answers. What changes is the
storage underneath, and the measurement is 326% faster at 100,000 rows.

## Where it stands

Everything here is measured, and each row links to the page carrying the run.

| | | |
|---|---|---|
| **326% faster than SQLite 3.53.4** | 4.26x weighted over ten workload families at 100,000 rows, four consecutive 30-round runs, every answer hashed and compared before its timing counts | [Performance](performance.md) |
| **67% less processor time** | 422 ms against 1,266 for the same plan, one child process each | [Performance](performance.md) |
| **15% more memory** | 42.6 MiB against 37.2, on the same 128 MiB budget. The one measurement SQLite wins | [Performance](performance.md#memory) |
| **A file within 4% of SQLite's** | 1.036x on the same imported data | [Performance](performance.md#disk) |
| **403 of 416 SQL cases byte for byte, none refused** | every case run through both shells over a fresh database and compared byte by byte | [SQL support](sql.md) |
| **Better than pgvector on 15 of 17 graded comparisons, worse on none** | both engines reading byte identical vectors | [Retrieval quality](retrieval-quality.md) |
| **175% faster unfiltered and 6,262% faster filtered** than pgvector | median in the calling process, against the correctly configured baseline | [Retrieval quality](retrieval-quality.md#latency) |

Six of the thirty timed workloads are slower than SQLite, and **no family is under the floor the
performance contract sets** — `transaction` was, on four consecutive runs, and is now 241% faster.
[The workloads that are slower](performance.md#the-workloads-that-are-slower) names each one and what
it costs.

## The case against PostgreSQL with pgvector

That combination is the sensible default, and it is the baseline every retrieval number here is
measured against: PostgreSQL for storage, pgvector for the vector index, and a server such as
llama.cpp holding the embedding model. It works. Four things about it are structural rather than a
matter of tuning.

**Filtering happens after the search rather than during it.** pgvector evaluates a `WHERE` clause
after the index scan has already chosen its candidates. A plain HNSW scan produces only
`hnsw.ef_search` of them, so a search restricted to a minority source can be left with almost none.
pgvector's answer is `hnsw.iterative_scan`, which keeps restarting the scan until enough rows pass,
and it costs latency: a filtered search that took a few milliseconds takes tens of them. Measured
here, 42.182 ms against 0.6631.

inillucent applies the filter inside the traversal. A node that fails the filter is still expanded,
so the walk can pass through it to reach what is behind it, but it is never admitted to the results.
The walk simply continues until it has collected enough passing rows.

**An exhaustive scan is often the right plan, and pgvector will not choose it.** When a filter admits
7,000 chunks out of 186,000, comparing the query against all 7,000 is both exactly correct and faster
than walking a graph over the whole corpus. inillucent counts what the filter admits, compares that
against a measured crossover, and takes the exhaustive scan when it wins. So a narrow filter is the
case where accuracy is perfect rather than the case where it collapses.

**The embedding model is a separate process reached over a socket.** Every query pays a process
boundary and an HTTP round trip before any searching happens, and the deployment has two things to
keep alive instead of one. inillucent runs the same model in the same process through the ONNX
runtime, at full precision.

**You are paying for durability, transactions, a planner and a wire protocol you are not using.** A
retrieval index is derived data: it is rebuilt from the source documents. Write ahead logging,
multiversion concurrency control, a cost based planner and a network protocol are cost with no return
on that workload. A 186,000 chunk corpus serves from under 2 GB, which fits on a laptop, and once the
index fits in memory everything PostgreSQL does to survive a power cut is overhead.

## What you give up

- **One writer at a time.** Readers never block it, and several processes can share one file under
  `PRAGMA locking_mode = normal`. Threads inside one process are not supported.
- **This engine's own file format.** SQLite files are imported once with
  [`inillucent migrate`](migrating.md), not opened in place.
- **No replication, no backups beyond a verified file copy, no wire protocol.** It is a library.
- **Adding content to a retrieval index rebuilds the graph**, on one thread: about nine and a half
  minutes over 598,560 passages.
- **Four workloads slower than SQLite**, listed on the performance page.
- **No macOS archive yet.** Build it with `cargo install inillucent-cli`.

## In production

A Gmail assistant with 598,560 passages across 64,172 messages and 2,287 attachments ran on
PostgreSQL with pgvector and PostgreSQL full text search, and now runs on inillucent with no other
database in the process:

| | before | after |
|---|---|---|
| median semantic search | 33.7 ms warm, 80.6 ms cold | **4.41 ms** |
| recall of the correct top 100 | 0.899 | **1.000** |
| real questions the keyword branch answered with nothing | 57% | **0%** |
| index inside the database | 3,167 MB of a 5,849 MB database | deleted |

The table above is the **retrieval** move. A second move took the **record** off PostgreSQL as well
— 16 tables and 1.63 million rows — by which time the same mailbox had grown to 602,022 passages.
That one found six query shapes that go quadratic on this engine, and one recovery failure to read
before a migration. Both are in
[Removing PostgreSQL from a 5.8 GB Gmail assistant](real-world-use-cases/nikaya-postgres-to-inillucent.md),
along with what each one cost and how it was fixed.

## Where to go next

- [Getting started](getting-started.md) — install it and run a query
- [Architecture](architecture.md) — how the retrieval engine works
- [SQL support](sql.md) — what runs, what differs, what is refused
- [Vector search](vector-search.md) — the retrieval engine from SQL and from the library
- [Roadmap](roadmap.md) — what is not there yet
