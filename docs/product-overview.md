# Product overview

**What Inillucent is, who it is for, and the case for it.** If you want to install it and run a
query, go to [Getting started](getting-started.md) instead.

## What it is

An embedded database written in Rust, in one library and one file.

It speaks SQLite's SQL on its own storage, and it holds a vector index and a keyword index in the
same file as your tables. One `.rdb` can carry ordinary rows, an HNSW graph over an embedding column
and a BM25 index over your text, and all three commit and roll back together.

There is no server to start, no port to configure, no connection string, and no network hop between
your application and its index. Four programs come out of an install: a command line, a shell that
works like `sqlite3`, an MCP server for AI agents, and a migrator that reads a SQLite file or a
running PostgreSQL or MySQL server.

**Several processes can use one database file at the same time.** They take the file's lock with the
same SHARED, RESERVED, PENDING and EXCLUSIVE protocol SQLite uses, under `PRAGMA locking_mode =
normal`, which is the default. Each connection reads the latest committed state when it takes the
lock, so a commit made by one process is visible to the next statement in another.
`crates/inillucent-compat/tests/process_concurrency.rs` runs two real writer processes against one
file and checks that the rows in it equal the commits that were acknowledged.

## Who it is for

**Somebody running a local AI agent.** The agent queries a body of written material: a wiki, a
repository, a set of tickets, a chat history. It needs both meaning and exact term, and it needs them
fast enough to run several searches inside one answer. Today that means PostgreSQL, the pgvector
extension, and an embedding model served over a socket. Inillucent replaces all three with a library.

**Somebody who already uses SQLite and wants it faster.** The SQL is the same: 404 of 416 probed
cases produce SQLite's exact bytes, nothing is refused, 6 answer differently, and 6 are vector search
features SQLite has no equivalent for. What changes is the storage underneath, and the measurement is
397% faster at 100,000 rows.

## Where it stands

Everything here is measured, and each row links to the page carrying the run.

| | | |
|---|---|---|
| **397% faster than SQLite 3.53.4** | 4.97x weighted over ten workload families at 100,000 rows, four consecutive 30-round runs with both engines on the performance cores, every answer hashed and compared before its timing counts | [Performance](performance.md) |
| **50% less processor time** | 555 ms against 1,082 for the same plan, one child process each | [Performance](performance.md) |
| **9.5% more memory** | 40.76 MiB against 37.22, on the same 128 MiB budget. The one measurement SQLite wins | [Performance](performance.md#memory) |
| **A file within 4% of SQLite's** | 1.036x on the same imported data | [Performance](performance.md#disk) |
| **404 of 416 SQL cases byte for byte, none refused** | every case run through both shells over a fresh database and compared byte by byte | [SQL support](sql.md) |
| **Better than pgvector on 15 of 17 graded comparisons, worse on none** | both engines reading byte identical vectors | [Retrieval quality](retrieval-quality.md) |
| **174% faster unfiltered and 6,169% faster filtered** than pgvector | median in the calling process, against the correctly configured baseline | [Retrieval quality](retrieval-quality.md#latency) |

Six of the thirty weighted workloads are slower than SQLite. So were all four correlated subquery
workloads the contract does not weight, by far more, in the graded run. At `52c4b5f`, on passes the
gate did not grade because the machine was busy, two of those four are faster than SQLite and a
correlated `EXISTS` over 400 outer rows takes 0.40 ms where it took 59.69; see
[Performance](performance.md#measured-again-at-52c4b5f-on-2026-09-24-and-not-graded). **Every family but one clears the 1.00x floor
the performance contract sets on all four runs**; `schema` went under it on three, with lower bounds
of 0.81x to 0.95x against a 1.31x ratio - it is a family of one workload, with the widest interval on
the page. The `transaction` family missed the floor on all four runs of two earlier measurements and now
measures 137% faster.
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
here, 35.583 ms against 0.6262.

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

- **One writer at a time.** Processes share the file, but writes take turns. A second writer waits
  up to `PRAGMA busy_timeout` and is then refused with `busy`. A reader waits for a writer as well,
  because there is no shared memory log index a reader could use to read a snapshot past it. Threads
  inside one process are not supported yet.
- **This engine's own file format.** SQLite files are imported once with
  [`inillucent migrate`](migrating.md), not opened in place.
- **No replication, no backups beyond a verified file copy, no wire protocol.** It is a library.
- **Write latency on a table with a vector index rises with its delta log, not with the corpus.**
  Adding content folds each new row into a segment, one graph insert per row written, and publishing
  a segment is proportional to the batch. The default delta log is 1,024 entries; a table that needs
  its write latency pinned declares `compact = N`. [Closed items](closed-items.md#a-generation-is-one-blob)
  has the measurements and
  [Keeping a vector index current](relational-architecture.md#10-keeping-a-vector-index-current) how
  to choose `N`.
- **Six of the thirty workloads are slower than SQLite**, listed on
  [the performance page](performance.md#the-workloads-that-are-slower).

## Where to go next

- [Getting started](getting-started.md): install it and run a query
- [Architecture](architecture.md): how the retrieval engine works
- [SQL support](sql.md): what runs, what differs, what is refused
- [Vector search](vector-search.md): the retrieval engine from SQL and from the library
- [Roadmap](roadmap.md): what is not there yet
