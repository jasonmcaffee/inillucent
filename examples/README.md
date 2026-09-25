# Examples

Each folder here is a complete project that uses inillucent the way an application outside this
repository does. The command line comes from a package manager or an install script, and the Rust
crates come from crates.io. No example is part of the repository's Cargo workspace or its test run,
so you can copy any folder out of the repository and it still works.

## What is here

| Folder | What it shows | What it uses |
|---|---|---|
| [`rag-agent/`](rag-agent/README.md) | a coding agent answering questions about Greek and Roman philosophy from a database, built two ways | the two folders below and a shared corpus |
| [`rag-agent/cli-example/`](rag-agent/cli-example/README.md) | a database that is already built and embedded. The agent searches it by running the `inillucent` command line, guided by `AGENTS.md` | the `inillucent` command line |
| [`rag-agent/rust-example/`](rag-agent/rust-example/README.md) | an MCP server written in Rust. It cuts the articles into overlapping chunks, embeds them, keeps the database in step with the source on a timer, and offers four kinds of search as MCP tools | the `inillucent` crate from crates.io |
| [`rag-agent/corpus/`](rag-agent/corpus/ATTRIBUTION.md) | 80 Wikipedia articles, one per line of `greek-philosophy.jsonl`, with their licence and sources | read by both examples |
| [`coffee-shop/`](coffee-shop/README.md) | a coffee shop's till and back office, written in Rust. Orders with sizes and modifiers, promotion codes, loyalty points, split payments, refunds, deliveries, waste, stock counts, a tip pool and closing the day, with the stock and a double entry ledger kept by triggers. The README shows each use case's SQL and its answer | the `inillucent` crate from crates.io |
| [`todo-mvc/`](todo-mvc/README.md) | a todo service with a REST API, written in Rust. Lists, todos with subtasks to any depth, tags, comments, a history written by triggers, keyword search and reports, with the SQL for each explained | the `inillucent` crate from crates.io |

## Which one to start with

| You want to | Start with |
|---|---|
| ask an agent questions in the next five minutes, with nothing to build | `rag-agent/cli-example/` |
| see how to give an agent a search tool over your own documents | `rag-agent/rust-example/` |
| compare vector search with inillucent's combined keyword and vector search | `rag-agent/rust-example/`, section "Vector column or inillucent_search table" |
| see how to keep a search index up to date as documents change | `rag-agent/rust-example/`, section "Keeping the index in step with the source" |
| build an ordinary web service on inillucent, with joins, foreign keys, triggers, recursive CTEs and window functions | `todo-mvc/` |
| keep money, stock and books that must agree to the cent, with triggers, generated columns, `UPSERT`, `UPDATE ... FROM` and reports built from window functions | `coffee-shop/` |

## What every example needs

The two `rag-agent` examples need the `inillucent` command line, for its `setup-embeddings`
command, and the embedding model it installs:

```sh
npm install -g inillucent          # or brew, pip, cargo, or an install script
inillucent setup-embeddings all    # about 620 MB, once per machine
```

The [top level README](../README.md#install) lists every way to install the command line. The Rust
examples also need a Rust toolchain from [rustup.rs](https://rustup.rs). `todo-mvc/` and `coffee-shop/`
need only the toolchain, because they use no embedding model.
