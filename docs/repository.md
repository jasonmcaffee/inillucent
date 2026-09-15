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
| shared foundation | `inillucent-base`, `inillucent-vfs`, `inillucent-value`, `inillucent-alloc` (the counting allocator the memory bar is measured through), `inillucent-sim` | 15,431 |
| shared SQL front end | `inillucent-sql` (lexer, parser, binder, planner), `inillucent-scalar` (functions, JSON, window frames), `inillucent-catalog`, `inillucent-ext` (registry, virtual table contract, FTS5, R-Tree) | 37,400 |
| the engine | `inillucent-pool`, `inillucent-wal`, `inillucent-tree`, `inillucent-txn`, `inillucent-exec`, `inillucent-engine`, `inillucent-model` (a test oracle), `inillucent-sqlite-reader` (import only) | 54,341 |
| kept for reading SQLite files | `inillucent-storage`, `inillucent-transaction` — the old engine's pager and transaction manager, kept because `inillucent-sqlite-reader` reads a SQLite file through them and migrating away from SQLite is what that reader is for | 18,764 |
| retrieval | `inillucent-core` (the engine), `inillucent-search` (the virtual table), `inillucent-bench` (the grading harness) | 29,825 |
| facade and tooling | `inillucent` (a re-export of the engine), `inillucent-compat` (the manifest, the oracle, the gates, 77 test files), `inillucent-cli`, `inillucent-migrate`, `inillucent-remote` | 32,031 |

**`inillucent-driver` is the public Rust API, and `inillucent` is a name for it.** `cargo add
inillucent` gives `pub use inillucent_driver::*;` and nothing else: `Database::open`,
`Database::session`, `Connection::query`, `Connection::prepare`, `Connection::begin` and the
`Transaction` that rolls back when it is dropped. There were two public surfaces over one engine
until task-1962, with different `Value`, `Error` and `Statement` types and nothing saying which to
depend on; the driver won because it has the transaction, the `Rows` type, the cancel flag and the
capability table checked in both directions, and because the C ABI and the four language packages
already reach the engine through it.

`inillucent-engine::connect::Database` is what the driver is built on and what `inillucent-cli`
drives directly: `open` creates or opens and recovers, `import` reads a SQLite file, and `session`
gives a connection with `execute_batch`, `query`, `prepare_with_tail`, `explain` and `begin`. It is
called `session` rather than `connect` because two of them share one transaction, which `connect`
reads as denying.

The old engine was the one that reached SQLite file format parity: 264 of 271 capabilities passed,
with seven optional ones missing. It was measured between 30% and 95% slower than SQLite across the
families, which is why the current engine was written, and it has now been deleted -
[Closed items](closed-items.md#the-old-engine-is-deleted) records what its four crates were and what still
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

**No test fails today.** `inillucent-testrun --strict` reports 170 targets, 2,819 tests, 0 failed
and 0 undetermined. The counts are exact. The wall clock was 840 seconds on a 24 processor desktop
that was carrying other work while it ran, so read it as one run on one machine rather than as a
figure to plan against. It still prints `not ok`, because five suites
evidenced nothing: `inillucent-remote::live_postgres` and `inillucent-remote::live_mysql` have no
server configured on this machine, `inillucent-remote::lib` runs only when
`INILLUCENT_NETWORK_TESTS` is set, because it opens sockets, and `inillucent-core::lib` and
`inillucent-bench` hold twenty-nine cases that need the embedding weights, which
`inillucent setup-embeddings all` installs. That is the condition `--strict` exists
to report, and `tools/doc-facts/check.mjs` accepts those four prerequisites and no others.

The last two joined the list in task-1913 and are not a new absence. Those twenty-nine cases sit
behind the `onnx` cargo feature, which the runner did not turn on, so they were in no binary at all
and nothing reported them - the source read as coverage while no run had ever started them.
`tests/selection.toml` now names the features a target is built with, so they are built, they run,
and the ones that need the weights say so. This page used to say seventeen tests failed; task-1869 had
already removed the cause and nobody re-ran it, which is recorded in
[Closed items](closed-items.md#what-task-1911-closed).

## What the tests cover

2,819 tests across 170 test targets in the workspace, in these classes:

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
- **Eight fuzz targets** over the codecs, and a seeded twin of every one of them that runs under
  `cargo test` on the pinned compiler. libFuzzer needs a nightly toolchain and a scheduled job, so a
  regression only a fuzz run finds is a regression that ships; the twins are in
  `crates/inillucent-base/tests/fuzz_seeded.rs`, `inillucent-tree`'s, `inillucent-pool`'s and
  `inillucent-wal`'s, and each sweeps twenty thousand deterministic inputs through the decoder and
  counts how many reached it, so a sweep that only ever exercised a refusal fails. The four
  non-codec targets - `json`, `mysql`, `postgres`, `store` - have had theirs beside the code since
  they were written.
- **The locking protocol across two real processes**, not two handles in one, because advisory locks
  are per process and a same process test would pass against a broken implementation. A dead process
  must release its locks.
- **One conformance suite run three ways** — against the in memory file system, the real one, and the
  simulator — so "the simulator behaves like a disk" is a checked claim rather than a hope.
- **100% branch coverage** held on the page pool's interior, latch, meta, extent, free map and swip
  modules, and on the tree's key codec.
- **28 of the 29 crates deny `unwrap`, `expect`, `panic` and slice indexing**, and 21 forbid
  `unsafe`, on every path that reads SQL text, database pages, log frames, network bytes or file
  system results. The twenty-ninth is `inillucent-bench`, which has no library to put the attributes
  in.

### How much of it is covered

Measured on 2026-09-15 at commit `37695de`, with `tools/validate.ps1 -Coverage`
(`tools/validate.sh --coverage` on Unix), which runs every suite under
`cargo llvm-cov` and prints this table. Region and line coverage, not branch:
branch coverage needs `-Z coverage-options=branch`, a nightly option, and
`rust-toolchain.toml` pins the compiler to stable for the reason written beside
the pin.

| crate | regions | region coverage | lines | line coverage |
|---|---:|---:|---:|---:|
| `inillucent-compat` | 32,460 | 40.9% | 19,717 | 42.7% |
| `inillucent-exec` | 21,917 | 89.7% | 12,896 | 91.5% |
| `inillucent-sql` | 20,966 | 86.0% | 12,937 | 88.0% |
| `inillucent-engine` | 17,242 | 87.6% | 10,922 | 89.2% |
| `inillucent-tree` | 16,992 | 92.0% | 8,465 | 93.8% |
| `inillucent-storage` | 14,417 | 83.0% | 7,761 | 83.2% |
| `inillucent-cli` | 13,087 | 47.9% | 7,731 | 49.5% |
| `inillucent-scalar` | 11,724 | 84.8% | 6,441 | 84.9% |
| `inillucent-ext` | 10,877 | 83.8% | 6,269 | 83.7% |
| `inillucent-remote` | 8,145 | 80.8% | 4,609 | 78.5% |
| `inillucent-pool` | 8,062 | 93.3% | 4,176 | 94.2% |
| `inillucent-transaction` | 6,193 | 85.3% | 3,143 | 88.5% |
| `inillucent-search` | 5,481 | 82.0% | 3,300 | 82.4% |
| `inillucent-value` | 5,425 | 95.4% | 3,073 | 95.6% |
| `inillucent-vfs` | 4,539 | 80.3% | 2,572 | 80.9% |
| `inillucent-base` | 4,376 | 88.7% | 2,348 | 89.0% |
| `inillucent-migrate` | 4,330 | 77.5% | 2,418 | 79.4% |
| `inillucent-catalog` | 3,458 | 78.2% | 2,068 | 80.4% |
| `inillucent-wal` | 2,650 | 93.7% | 1,683 | 96.7% |
| `inillucent-txn` | 2,470 | 89.8% | 1,432 | 87.4% |
| `inillucent-sim` | 2,146 | 95.2% | 1,317 | 95.7% |
| `inillucent-driver` | 1,379 | 70.8% | 910 | 70.4% |
| `inillucent-driver-capi` | 1,108 | 0.8% | 847 | 0.9% |
| `inillucent-sqlite-reader` | 432 | 84.5% | 228 | 87.3% |
| `inillucent-alloc` | 355 | 91.3% | 165 | 84.8% |
| **total** | **220,231** | **77.2%** | **127,428** | **77.8%** |

Three rows need reading rather than ranking.

`inillucent-driver-capi` reads 0.8%, and the C ABI is not untested: its
conformance suite drives the symbols through a C program that links the built
`cdylib`, which is a separate binary from the instrumented test executables this
measurement merges. What the number says is that no Rust test calls those
functions, which is true and is what a C ABI is for.

`inillucent-compat` at 40.9% and `inillucent-cli` at 47.9% are the two crates
that are mostly *programs*: eighteen gate and profiling binaries between them,
each run by hand or by a scheduled job rather than by `cargo test`. The library
halves of both are covered by the suites that use them.

The three retrieval crates - `inillucent-core`, `inillucent-bench` and
`inillucent-model` - are excluded from the run. They need ONNX Runtime and a
corpus, and on a machine without either they contribute uninstrumented zeros
rather than a number.

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
