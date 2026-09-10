# AGENTS.md — working with inillucent, for an AI agent

This is the shortest path to being useful here. Two audiences, and the split is the first thing to
get right:

- **You are USING inillucent** — putting a database in an application, querying one, migrating one
  in. Read [§1](#1-using-inillucent) and stop. `agent-skills/` has one page per job.
- **You are WORKING ON inillucent** — changing this repository. Read all of it. There are five
  contracts here that a test enforces, and every one of them fails a build when you guess.

`agent-skills/README.md` is the index of the skills. If a task is on that list, open that page
first; it is shorter than this file and it is written for exactly that job.

---

## 1. Using inillucent

inillucent is an embedded SQL database in Rust with two engines in one file: a **relational** one
that speaks SQLite's dialect on its own storage, and a **retrieval** one — HNSW vectors and BM25 —
reachable from that same SQL. One `.rdb` can hold ordinary tables and a hybrid index that commits and
rolls back with them. It links no other database.

Four programs come out of a build or an install:

| | |
|---|---|
| `inillucent` | the command line: 30 commands, with `--output json` on all of them |
| `inillucent-shell` | an interactive shell shaped like `sqlite3`, with 63 of its dot commands |
| `inillucent-mcp` | the same commands served to an agent over MCP |
| `inillucent-migrate` | builds a database from a SQLite file, a PostgreSQL or MySQL server, or a legacy retrieval index |

```sh
inillucent create app.rdb
inillucent --db app.rdb exec "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)"
inillucent --db app.rdb exec "INSERT INTO note (body) VALUES (?1)" --params '["hello"]'
inillucent --db app.rdb query "SELECT * FROM note" --output json
inillucent help                 # the whole table
inillucent help migrate         # one command, every parameter
```

### The four things that will save you a wrong turn

1. **Exit code 3 means "this engine has not built that".** It is a *different* code from 1, on
   purpose, so a script can branch on "not yet" without matching on a message. Over the driver and
   MCP the same thing is the status `unsupported`. Do not treat it as a syntax error and start
   rewording your SQL — it will not help.
2. **Ask before you compose.** `inillucent capabilities` enumerates what the engine does, and every
   row is checked against the running engine by a test **in both directions** — a claimed capability
   that fails and a denied one that now works each turn the build red. That makes it worth trusting
   in a way a hand-written feature list is not.
3. **`--output json` on any command** gives the same object a language binding sees: typed values, an
   exact `total` independent of `--limit`, and the driver's own status name on a failure. Parse that
   rather than the aligned table.
4. **Bind parameters, do not paste values.** `--params '["…"]'` binds `?1`, `?2` … in order. The
   quoting bug you avoid is the same one in every language.

### Where the answers are

| question | file |
|---|---|
| does *X* work? | [`docs/sql.md`](docs/sql.md), and [`docs/feature-comparison.md`](docs/feature-comparison.md) for the 416 measured cases |
| how does it compare to SQLite? | [`docs/performance.md`](docs/performance.md) |
| how does it compare to pgvector? | [`docs/retrieval-quality.md`](docs/retrieval-quality.md) |
| how do I search by meaning or by exact term? | [`docs/vector-search.md`](docs/vector-search.md) |
| how do I bind this from Python / Node / Go / PHP / C? | [`drivers/README.md`](drivers/README.md) |
| how does the retrieval half work? | [`docs/architecture.md`](docs/architecture.md) |
| how does the SQL half work? | [`docs/relational-architecture.md`](docs/relational-architecture.md) |
| what is not built yet? | [`docs/roadmap.md`](docs/roadmap.md) |
| where is everything? | [`docs/README.md`](docs/README.md), the documentation index |

---

## 2. Working on inillucent

### The five contracts, and the test that enforces each

Guessing at any of these produces a red build rather than a review comment. Read the contract before
you write the code; each one is short.

| contract | where it lives | what fails |
|---|---|---|
| **Dependencies** — an allowed list, not a denied one | `docs/dependency-policy.md`, `docs/invariants/layering.toml` | `cargo test -p inillucent-compat --test policy` |
| **Layering** — which crate may depend on which | `docs/invariants/layering.toml` | the same suite, `the_workspace_obeys_the_dependency_contract` |
| **Test selection** — every test target has a row | `tests/selection.toml` | `--test selection`; `no_test_hides_outside_the_map` names your target |
| **One command table** — the command line and MCP are generated from it | `crates/inillucent-cli/src/command/registry.rs` | `--test command_parity` |
| **The testing standard** — where a new test goes and how the suite runs | `tests/inillucent-testing-tdd.md` | — |

### Adding a dependency

**You probably cannot.** Production crates may not link another database engine, SQL parser, storage
engine, B-tree or LSM library, transaction manager, WAL, or query optimizer, and the check matches on
substrings so a rename does not slip past. What is allowed is *infrastructure*: error plumbing, the
operating-system boundary, numeric kernels — one `[[external]]` row each, naming every crate that may
use it.

Before reaching for a crate, read the two worked arguments in `docs/dependency-policy.md`: the
additions that were made, and **the dependency that was deliberately not added** — a PostgreSQL and MySQL
client, written here in `crates/inillucent-remote` rather than pulled in, and why.

### Running the tests

**Do not run `cargo test --workspace` while you iterate.** There is a parallel, selective runner:

```sh
cargo build -p inillucent-compat --bin inillucent-testrun --features testrun

target/debug/inillucent-testrun --tier smoke      # ~1 s, mid-edit
target/debug/inillucent-testrun --changed         # what your edits can break
target/debug/inillucent-testrun --changed --list  # ...without running it
target/debug/inillucent-testrun                   # everything, ~155 s
target/debug/inillucent-testrun --strict          # fail on a missing prerequisite
```

`--strict` matters: several suites need something the workspace cannot build — the pinned SQLite
oracle, a corpus, a live PostgreSQL — and they *report success* when it is absent. `--strict` counts
those and names them, so a green with nothing installed cannot be mistaken for a green.

### Writing a test that is worth having

The standard is `tests/inillucent-testing-tdd.md` and its six rules. The two that get broken most:

- **A test asserts a value, not the absence of a crash.** `assert!(result.is_ok())` on a migration
  that published nothing is a passing test of nothing.
- **A test that cannot fail is worse than no test.** A benchmark that excludes the change under test,
  a check whose prerequisite is missing, a gate whose bound is straddled — each reports green and
  means nothing.

New `tests/*.rs` file? Add its row to `tests/selection.toml`, or `selection.rs` fails and names it.

### House style

Read three neighbouring files before writing one. The conventions that carry weight:

- **Every function has a doc comment saying what it is for**, with `@param` lines. Governed crates
  `deny(missing_docs)` and a test checks that every module states its invariant.
- **Comments carry the argument, not the mechanics.** The valuable comment in this tree says *why
  the obvious thing is wrong* — what was measured, what failed before, what a different choice would
  cost. `// increment the counter` is noise; the paragraph in
  `crates/inillucent-remote/src/migrate.rs` explaining why the digest is a sum and not an
  exclusive-or is the house style.
- **A comment may only claim what its test proves.** This is rule 1.5 of the testing standard and it
  is enforced by review, not by a compiler.
- **No `unwrap`, `expect`, `panic!` or slice indexing** on any path that reads SQL text, database
  pages, journal frames, network bytes or VFS results. The governed crates `deny` all four and relax
  them only under `#[cfg(test)]`.
- `cargo fmt` before you finish — `policy.rs` fails on an unformatted governed crate.

### The shape of a finished change

1. The code, with its doc comments and its arguments.
2. Its tests, where §2.1 of the testing standard says they go, registered in `tests/selection.toml`.
3. `target/debug/inillucent-testrun --changed`, green.
4. `cargo fmt`.
5. Whatever contract file the change touches — a layering row, a dependency argument, a command-table
   entry — updated in the same commit, because each of those is checked by a test that will otherwise
   fail on somebody else's machine.
