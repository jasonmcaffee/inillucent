# inillucent documentation

The documentation in this repository. There is also a documentation book at
[inillucent.com/docs](https://inillucent.com/docs), written as a tutorial in 24 chapters — start
there if you are learning the engine rather than looking something up.

Every page here is written to be read on its own. Nothing is left implicit because an earlier page
said it.

**The order below is a reading order**, for somebody who has not written a database before: what it
is, then the words, then how to run it, then one page that shows both engines, then each engine in
depth. A page in `docs/` that is not in one of these tables fails
`cargo test -p inillucent-compat --test documentation`, so the index cannot drift from the
directory.

## Start here

| | page | what it answers |
|---|---|---|
| 1 | [What it is](product-overview.md) | what inillucent is, who it is for, and the case for it against PostgreSQL with pgvector |
| 2 | [Words to know](glossary.md) | every term this documentation uses that a general programmer would not already know, one sentence each |
| 3 | [Getting started](getting-started.md) | install it, run the four programs, make a database, read the exit codes |
| 4 | [Architecture in one page](architecture-overview.md) | both engines in one diagram, one query across both halves, and where the bytes live |

## The two engines

| | page | what it answers |
|---|---|---|
| 5 | [The retrieval engine](architecture.md) | how searching by meaning works, in plain terms and with no Rust in it |
| 6 | [The relational engine](relational-architecture.md) | the SQL half: storage, transactions, the log, recovery, backup, budgets and confinement, each with the test that checks it |

## Using it

| | page | what it answers |
|---|---|---|
| 7 | [SQL support](sql.md) | which SQL runs, which cases differ from SQLite, and what is refused by name |
| 8 | [Pragmas](pragmas.md) | every pragma this engine recognises, generated from the register the engine itself reads |
| 9 | [Vector search](vector-search.md) | `VECTOR(N)` columns, HNSW indexes, `inillucent_search`, and how hybrid ranking works |
| 10 | [Embeddings](embeddings.md) | running the embedding model in your process, on GPUs, and comparing models |
| 11 | [Migrating](migrating.md) | bringing in a SQLite file, a PostgreSQL server or a MySQL server |
| | [Where the vectors live](vector-residency.md) | held in memory or read from the file, and what each one costs |

## The measurements

| | page | what it answers |
|---|---|---|
| 12 | [Performance](performance.md) | against SQLite 3.53.4: speed, processor time, memory, disk, and the workloads that are slower |
| 13 | [Feature comparison](feature-comparison.md) | the full 416-case differential probe, feature by feature, plus the retrieval engine against pgvector |
| 14 | [Retrieval quality](retrieval-quality.md) | the 17 graded comparisons against pgvector, and how a measurement becomes a verdict |
| | [Synthetic corpus](../tests/synthetic-corpus.md) | building the public corpus every retrieval number is measured on |

`inillucent-scorecard.md` in the repository root is the score card a grading run writes, with every
interval, every p-value and every diagnostic. It stays at the root because
`inillucent-bench grade` writes it there, and `inillucent-scorecard.json` beside it holds the same
measurements so the card can be rendered or judged again without repaying the run. It and
[Retrieval quality](retrieval-quality.md) are the **same** run, taken 2026-09-19.

## Working on it

| | page | what it answers |
|---|---|---|
| 15 | [Repository and building](repository.md) | the crates, what each one is for, building, running the tests, and the coverage table |
| 16 | [Dependency policy](dependency-policy.md) | what a production crate may link, and why the allowed list is short |
| 17 | [Roadmap](roadmap.md) | what is not there yet, in the order it is being worked |
| | [Closed items](closed-items.md) | what came off the roadmap, with the measurement that closed each, and what is settled |
| | [`AGENTS.md`](../AGENTS.md) | the front door for an AI agent, using or changing this repository |
| | [`agent-skills/`](../agent-skills/README.md) | one task shaped page per job, readable by any agent |

`invariants/layering.toml` is the dependency contract a test enforces. It is a data file rather than
prose; [Dependency policy](dependency-policy.md) explains what it says.
