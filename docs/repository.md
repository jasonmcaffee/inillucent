# The repository

How the code is laid out, how to build it, and how to run the tests.

If you are changing this repository rather than reading it, [`AGENTS.md`](../AGENTS.md) §2 and
[`agent-skills/inillucent-develop`](../agent-skills/inillucent-develop/SKILL.md) are the shorter
pages: they carry the five contracts a test enforces, and guessing at any of them produces a red
build rather than a review comment.

## Building it

```sh
cargo build --release
```

That is the whole of it for the engine and the four programs. Two optional features need something
on the machine first:

| feature | needs |
|---|---|
| `onnx` — the embedding model in process | the ONNX Runtime shared library |
| `embed` — `embed(TEXT)` as a SQL function | the same, plus the weights |

Both are installed by one command, on any of the three platforms:

```sh
inillucent setup-embeddings all
```

which is also how a developer gets them: nothing has to be exported afterwards, because the engine
looks where the command put them. `ORT_DYLIB_PATH` and `INILLUCENT_ONNX_DIR` remain overrides for a
machine that already has a copy somewhere else — which is what this repository's own grading
harness uses, since its eight models live on a drive the installer would never write to.

[Embeddings](embeddings.md) covers both.

**On Windows**, Git Bash does not inherit the MSVC `INCLUDE` and `LIB` that `onig_sys` needs. Dump
them out of `vcvars64.bat` once and export them into the shell before `cargo build`.

## The crates

| group | crates | non test lines |
|---|---|---|
| shared foundation | `inillucent-base`, `inillucent-vfs`, `inillucent-value`, `inillucent-sim` | 15,431 |
| shared SQL front end | `inillucent-sql` (lexer, parser, binder, planner), `inillucent-scalar` (functions, JSON, window frames), `inillucent-catalog`, `inillucent-ext` (registry, virtual table contract, FTS5, R-Tree) | 37,400 |
| the engine | `inillucent-pool`, `inillucent-wal`, `inillucent-tree`, `inillucent-txn`, `inillucent-exec`, `inillucent-engine`, `inillucent-model` (a test oracle), `inillucent-sqlite-reader` (import only) | 54,341 |
| kept for reading SQLite files | `inillucent-storage`, `inillucent-transaction` — the old engine's pager and transaction manager, kept because `inillucent-sqlite-reader` reads a SQLite file through them and migrating away from SQLite is what that reader is for | 18,764 |
| retrieval | `inillucent-core` (the engine), `inillucent-search` (the virtual table), `inillucent-bench` (the grading harness) | 29,825 |
| facade and tooling | `inillucent` (a re-export of the engine), `inillucent-compat` (the manifest, the oracle, the gates, 77 test files), `inillucent-cli`, `inillucent-migrate`, `inillucent-remote` | 32,031 |

`inillucent-engine::connect::Database` is the entry point: `open` creates or opens and recovers,
`import` reads a SQLite file, and `connect` gives a connection with `execute_batch`, `query`,
`prepare_with_tail` and `explain`. `inillucent::Database` is a re-export of it — the two surfaces do
not differ, so there is no wrapper.

The old engine was the one that reached SQLite file format parity: 264 of 271 capabilities passed,
with seven optional ones missing. It was measured between 30% and 95% slower than SQLite across the
families, which is why the current engine was written, and it has now been deleted -
[Roadmap](roadmap.md#7-the-old-engine-is-deleted) records what its four crates were and what still
reads a SQLite file in their place.

## The other directories

| | |
|---|---|
| `drivers/` | the sub project an application binds to. `drivers/inillucent-driver` holds every decision, `drivers/inillucent-driver-capi` is the C ABI over it as both a dynamic and a static library, and `drivers/README.md` is the front door for somebody writing a binding who is not working on the engine |
| `compat/` | the pinned SQLite manifest, the source register, the fixtures every differential suite reads, and the performance contract in `compat/perf/contract.toml` |
| `tests/` | the shared corpora, the crash schedules, the workload traces, the test selection map, and [the testing standard](../tests/inillucent-testing-tdd.md) |
| `tools/` | the pinned reference build, the feature probe, and the gate fixture builder |
| `packaging/` | how a release is cut, and what a signed installer would take on each platform |
| `agent-skills/` | one task shaped page per job, for an AI agent |
| `examples/` | a worked example per thing that is hard to evaluate from a document. `examples/rag-agent/` is a Greek philosophy database, already embedded and committed, that an agent can search in its first minute — it is the one place a `.rdb` and a corpus are tracked, and the `.gitignore` rules say why |
| `fuzz/` | eight libFuzzer targets over the codecs, built and run on their own |
| `docs/invariants/layering.toml` | the dependency contract, enforced by a test |

## Running the tests

**Do not run `cargo test --workspace` while you iterate.** There is a parallel, selective runner:

```sh
cargo build -p inillucent-compat --bin inillucent-testrun --features testrun

target/debug/inillucent-testrun --tier smoke      # about 1 s, for mid edit
target/debug/inillucent-testrun --changed         # what your edits can break
target/debug/inillucent-testrun --changed --list  # ...without running it
target/debug/inillucent-testrun                   # everything, about 300 s
target/debug/inillucent-testrun --strict          # fail on a missing prerequisite
```

**`--strict` matters.** Several suites need something the workspace cannot build — the pinned SQLite
oracle, a corpus, a live PostgreSQL — and without it they *report success* when that thing is absent.
`--strict` counts them and names them, so a green run on a machine with nothing installed cannot be
mistaken for a green run.

If you do use `cargo test --workspace`, pass `--no-fail-fast`. Without it the run stops at the first
failing binary, and has reported about a quarter of the suite.

**No test fails today.** `inillucent-testrun --strict` on a quiet box reports 149 targets, 2,646
tests, 0 failed and 0 undetermined in 301 seconds. It still prints `not ok`, because `live_postgres`
and `live_mysql` evidenced nothing and neither server is configured on this machine — which is the
condition `--strict` exists to report. This page used to say seventeen tests failed; task-1869 had
already removed the cause and nobody re-ran it, which is recorded in
[the roadmap](roadmap.md#what-task-1911-closed).

## What the tests cover

2,646 tests across 149 test targets in the workspace, in these classes:

- **A differential harness** that runs the same SQL through the pinned SQLite 3.53.4 and compares
  transcripts. 208 of those cases are `semantics.rs`, and 416 are the wider feature probe.
- **A SQLLogicTest subset**, whose expected values were recorded from the pinned binary — so the
  suite grades this engine against SQLite on a machine that has no SQLite on it. Nothing in the
  generator reads inillucent: a corpus that recorded the engine's own answer as the thing to grade
  against would test nothing.
- **A `BTreeMap` model reference**, driven by operation traces.
- **A deterministic fault injecting file system** under the page pool, the log and the transaction
  engine. It must lose an unsynced write sometimes and a synced one never, over every seed, and a
  recorded schedule must replay an identical trace event for event.
- **Crash campaigns**, in which a crash on either side of a checkpoint or a log retirement has to
  recover the same database.
- **Eight fuzz targets** over the codecs. Every codec is also exercised with hundreds of thousands of
  seeded random inputs and must return an error rather than panic on any of them.
- **The locking protocol across two real processes**, not two handles in one, because advisory locks
  are per process and a same process test would pass against a broken implementation. A dead process
  must release its locks.
- **One conformance suite run three ways** — against the in memory file system, the real one, and the
  simulator — so "the simulator behaves like a disk" is a checked claim rather than a hope.
- **100% branch coverage** held on the page pool's interior, latch, meta, extent, free map and swip
  modules, and on the tree's key codec.
- **26 of the 29 crates deny `unwrap`, `expect`, `panic` and slice indexing**, and 22 forbid
  `unsafe`, on every path that reads SQL text, database pages, log frames, network bytes or file
  system results.

## The contracts a test enforces

| contract | where it lives | what fails |
|---|---|---|
| **Dependencies** — an allowed list, not a denied one | [`docs/dependency-policy.md`](dependency-policy.md) | `cargo test -p inillucent-compat --test policy` |
| **Layering** — which crate may depend on which | `docs/invariants/layering.toml` | the same suite, `the_workspace_obeys_the_dependency_contract` |
| **Test selection** — every test target has a row | `tests/selection.toml` | `--test selection`, which names your target |
| **One command table** — the command line and MCP are generated from it | `crates/inillucent-cli/src/command/registry.rs` | `--test command_parity` |
| **The testing standard** — where a new test goes, and how the suite runs | [`tests/inillucent-testing-tdd.md`](../tests/inillucent-testing-tdd.md) | reviewed rather than compiled |

## Reproducing the measurements

```sh
# the pinned SQLite 3.53.4 oracle
pwsh tools/sqlite-reference.ps1      # Windows
bash tools/sqlite-reference.sh       # Linux

# the performance gates
bash tools/build-gate-fixtures.sh <dir>
cp <dir>/medium.db <dir>/medium-run1.db
target/release/inillucent-fullgate <dir>/medium-run1.db --scale medium --rounds 30 \
    --page-size 32768 --frames 4096
target/release/inillucent-readgate    <dir>/medium-read.db --scale medium
target/release/inillucent-shellrss
target/release/inillucent-vectorprobe --rows 20000 --dims 256
# a report, not a gate: it prints the retrieval consumer's absolute cost on
# this engine's own storage and always exits 0 unless something actually
# errors. It used to compare against the old engine's storage; that engine is
# deleted and no recorded floor exists to gate against in its place, so it
# reports rather than passing or failing.
target/release/inillucent-searchgate  --documents 500 --rounds 30

# the parity manifest and the dependency contract
cargo run -p inillucent-compat --bin inillucent-manifest -- check
cargo run -p inillucent-compat --bin inillucent-manifest -- report
cargo run -p inillucent-compat --bin inillucent-manifest -- layering
```

[Performance](performance.md#reproducing-it) has the settings each gate is run under and why.
[Retrieval quality](retrieval-quality.md#running-it) has the graded comparison, and
[Synthetic corpus](../tests/synthetic-corpus.md) builds the corpus it runs on.

## House style

Read three neighbouring files before writing one. The conventions that carry weight:

- **Every function has a doc comment saying what it is for**, with `@param` lines. Governed crates
  `deny(missing_docs)`, and a test checks that every module states its invariant.
- **Comments carry the argument, not the mechanics.** The comment worth writing here says why the
  obvious thing is wrong: what was measured, what failed before, what a different choice would cost.
- **A comment may only claim what its test proves.**
- **A test asserts a value, not the absence of a crash.** `assert!(result.is_ok())` on a migration
  that published nothing is a passing test of nothing.
- **A test that cannot fail is worse than no test.** A benchmark that excludes the change under test,
  a check whose prerequisite is missing, a gate whose bound is straddled — each reports green and
  means nothing.
- `cargo fmt` before you finish. `policy.rs` fails on an unformatted governed crate.
