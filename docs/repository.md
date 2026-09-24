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
until the driver was unified into a single Rust API, with different `Value`, `Error` and `Statement`
types and nothing saying which to depend on; the driver won because it has the transaction, the
`Rows` type, the cancel flag and the
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

**One test fails today, and it is a pinned checksum rather than a behaviour.**
`inillucent-testrun --strict` reports 1 failed over the 231 rows in `tests/selection.toml`:
`harness::the_retrieval_baseline_is_unchanged`, which pins the retrieval engine's source files by
checksum so that work on the relational engine cannot disturb them. The design for a faster commit
path deliberately changed three of those files - the distance kernel, the graph build and the index -
and the amendment that records each file, its ticket and its new digest is written when that design
is finished. The
wall clock was 3,250 seconds on a 24 processor desktop, so read it as one run on one machine rather
than as a figure to plan against.

**It will still print `not ok` on your machine, and how many suites it names depends on what you
have installed.** This page used to answer that with a list of the five suites one run on one
desktop happened to name, which told a reader on a fresh clone nothing: a machine without the
oracle, the pinned shell, a C compiler, Python with `ssl`, or `openssl` sees thirty or forty, and
there was no way to tell an expected absence from a new one. So what is published is the shape
instead - every prerequisite any row declares, how many rows declare it, and what provides it. It
is read out of `tests/selection.toml`, and
`cargo test -p inillucent-compat --test documentation` fails when a value in the map is not in this
table.

<!-- requires:begin -->

| prerequisite | rows | what provides it |
|---|---:|---|
| `oracle` | 31 | the pinned SQLite 3.53.4 comparison process: `pwsh tools/sqlite-reference.ps1`, `bash tools/sqlite-reference.sh` |
| `shell` | 9 | the pinned `sqlite3` 3.53.4 shell, from the same two scripts as the oracle |
| `tracked-fixtures` | 5 | the files under `compat/fixtures/`, which are in the repository - declared for a checkout that has lost them, not for a fresh clone |
| `onnx` | 3 | ONNX Runtime and the embedding weights: `inillucent setup-embeddings all` |
| `python` | 3 | a Python interpreter with `ssl`, for the TLS server, the `ctypes` conformance runner and the workload extractor |
| `fixtures` | 2 | the gate fixtures, which are 1.2 MB and 120 MB and are not tracked: `bash tools/build-gate-fixtures.sh _agent_output/fixtures` |
| `node` | 1 | a Node.js runtime, for the npm wrapper's conformance runner: https://nodejs.org/ |
| `go` | 1 | a Go toolchain, for the Go wrapper's conformance runner: https://go.dev/dl/ |
| `php` | 1 | a PHP interpreter, for the PHP wrapper's conformance runner: https://www.php.net/downloads |
| `asan` | 1 | a toolchain with the address sanitizer, which is nightly on every platform and absent on Windows |
| `embed` | 1 | a build with `inillucent-engine/embed` compiled in, which is what registers `embed(TEXT)` as a name to refuse. The runner builds it from the target's `features` row; `tools/coverage.mjs` does not, because the feature reaches `inillucent-core/onnx` and that crate is excluded from the coverage run |
| `baseline` | 1 | a recorded performance baseline: `cargo run -p inillucent-compat --bin inillucent-baseline -- capture` |
| `btree-corpus` | 1 | the retained sequences under `compat/corpus/btree/`, which are tracked |
| `cc` | 1 | a C compiler on `PATH`, for the program that links the C ABI |
| `conformance-records` | 1 | what the five conformance runners recorded under `_agent_output/conformance/`: `sh tools/run-package-tests.sh` |
| `directory-link` | 2 | permission to create a directory link, which Windows gives an elevated shell or a machine in developer mode |
| `local-timezone` | 1 | a configured local time zone the operating system will convert an instant through: `localtime_r` on Unix, `SystemTimeToTzSpecificLocalTime` on Windows |
| `mysql` | 1 | a live MySQL server, named by `INILLUCENT_TEST_MYSQL_URL` |
| `narrow-slots` | 1 | the narrow integer slots compiled in, which is a constant in `crates/inillucent-tree/src/leaf.rs` |
| `network` | 1 | outbound network access, turned on by setting `INILLUCENT_NETWORK_TESTS` |
| `nikaya` | 1 | a local checkout of the application the replay workload is extracted from. The extract is tracked, so this is only needed to check it for staleness |
| `openssl` | 1 | the `openssl` command, which generates the certificates the TLS suite serves |
| `postgres` | 1 | a live PostgreSQL server, named by `INILLUCENT_TEST_POSTGRES_URL` |
| `previous-release` | 1 | a published release's binary, downloaded and verified by `pwsh tools/build-interop-fixture.ps1 -Version <version>` into the gitignored `tools/cross/bin/releases/` |
| `sqlite-bench` | 1 | the pinned benchmark driver, built by the same two reference scripts |
| `testrun` | 1 | the runner itself: `cargo build -p inillucent-compat --bin inillucent-testrun --features testrun`, which a plain `cargo test` does not build |

<!-- requires:end -->

A row without a prerequisite is a suite that runs everywhere. A suite that can skip and does not
declare one fails `cargo test -p inillucent-compat --test selection`, and so does a row that
declares one whose suite cannot skip - which is what keeps this table equal to the workspace rather
than equal to the last time somebody looked.

The last two joined the list during a differential bug hunt that found tests that were not running,
and are not a new absence. Those twenty-nine cases sit
behind the `onnx` cargo feature, which the runner did not turn on, so they were in no binary at all
and nothing reported them - the source read as coverage while no run had ever started them.
`tests/selection.toml` now names the features a target is built with, so they are built, they run,
and the ones that need the weights say so. This page used to say seventeen tests failed; an earlier
fix had already removed the cause and nobody re-ran it, which is recorded in
[Closed items](closed-items.md#eight-items-closed-together).

## What the tests cover

3,499 tests across 231 test targets in the workspace, in these classes:

The 231 is the `[[target]]` row count in `tests/selection.toml`, which is what
`tools/doc-facts/check.mjs` compares this sentence against and what the runner is asked to run.
The number of `#[test]` attributes in the tree is 3,240, and it differs from the run's count in
both directions. `scenario!` writes six tests from one line, so a story file holds no attribute at
all for the six it contributes. The other way, a `#[cfg(windows)]` and a `#[cfg(unix)]` pair is two
attributes and one test on any one machine, and five `onnx` cases are built only when that feature
is on.

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
- **The page pool and the tree are the two most covered crates in the workspace**, at 93.4% and
  92.0% of regions and 94.3% and 93.8% of lines - the page pool being the interior, latch, meta,
  extent, free map and swip modules, and the tree being the key codec among them.

  This sentence used to claim complete **branch** coverage of those modules, sixteen lines above the
  sentence saying branch coverage cannot be measured on the pinned toolchain. Both cannot be true,
  and it is the second one that is: branch coverage needs `-Z coverage-options=branch`, a nightly
  option, and `rust-toolchain.toml` pins stable. What replaced it is the two crate numbers from the
  table below, which is what `tools/coverage.mjs` actually measures. **Per-module numbers are not
  published**, because `tools/coverage.mjs` aggregates to the crate and nothing here has measured
  them.
- **29 of the 29 crates deny `unwrap`, `expect`, `panic` and slice indexing**, and 21 forbid
  `unsafe`, on every path that reads SQL text, database pages, log frames, network bytes or file
  system results. The twenty-ninth to arrive was `inillucent-bench`, when the bench crate was brought
  under the same four lints: it is a binary
  crate, and the attributes go on `main.rs` because a `#![deny(..)]` is a crate root inner attribute
  and `main.rs` is a crate root. Turning them on there produced 191 errors - 154 slice indexes, 18
  slices, 8 `unwrap`s and 11 `expect`s - in the harness that scores the numbers on this page and in
  `docs/retrieval-quality.md`.

### How much of it is covered

Measured on 2026-09-15 at commit `f9d1433`, which is `v0.1.3`, with
`tools/validate.ps1 -Coverage` (`tools/validate.sh --coverage` on Unix), which
runs every suite under `cargo llvm-cov` and prints this table. **The repository
is at `v0.1.4` and this table has not been measured again since `v0.1.3`**, so
read it as the last measurement rather than as the current one; the command
above is what refreshes it, and it takes about an hour. Region and line
coverage, not branch: branch coverage needs `-Z coverage-options=branch`, a
nightly option, and `rust-toolchain.toml` pins the compiler to stable for the
reason written beside the pin.

<!-- coverage:begin -->

| crate | regions | region coverage | lines | line coverage |
|---|---:|---:|---:|---:|
| `inillucent-compat` | 32,756 | 40.6% | 19,940 | 42.3% |
| `inillucent-exec` | 22,980 | 90.3% | 13,552 | 92.0% |
| `inillucent-sql` | 21,174 | 86.1% | 13,065 | 88.2% |
| `inillucent-engine` | 17,737 | 87.3% | 11,377 | 88.9% |
| `inillucent-tree` | 16,992 | 92.0% | 8,465 | 93.8% |
| `inillucent-cli` | 13,208 | 48.4% | 7,836 | 50.1% |
| `inillucent-storage` | 14,409 | 82.9% | 7,751 | 82.8% |
| `inillucent-scalar` | 11,724 | 84.8% | 6,441 | 84.9% |
| `inillucent-ext` | 10,877 | 83.8% | 6,269 | 83.7% |
| `inillucent-remote` | 8,145 | 80.8% | 4,609 | 78.5% |
| `inillucent-pool` | 8,062 | 93.4% | 4,176 | 94.3% |
| `inillucent-search` | 5,481 | 82.0% | 3,300 | 82.4% |
| `inillucent-transaction` | 6,193 | 85.3% | 3,143 | 88.5% |
| `inillucent-value` | 5,425 | 95.4% | 3,073 | 95.6% |
| `inillucent-vfs` | 4,539 | 80.3% | 2,572 | 81.1% |
| `inillucent-migrate` | 4,330 | 77.5% | 2,418 | 79.4% |
| `inillucent-base` | 4,376 | 88.7% | 2,348 | 89.0% |
| `inillucent-catalog` | 3,458 | 78.2% | 2,068 | 80.4% |
| `inillucent-wal` | 2,650 | 93.7% | 1,683 | 96.8% |
| `inillucent-txn` | 2,527 | 89.8% | 1,464 | 88.0% |
| `inillucent-sim` | 2,146 | 95.0% | 1,317 | 95.4% |
| `inillucent-driver` | 1,806 | 65.1% | 1,185 | 64.6% |
| `inillucent-driver-capi` | 1,112 | 0.8% | 851 | 0.9% |
| `inillucent-sqlite-reader` | 432 | 86.6% | 228 | 89.9% |
| `inillucent-alloc` | 355 | 91.3% | 165 | 84.8% |
| **total** | **222,894** | **77.2%** | **129,296** | **77.8%** |

<!-- coverage:end -->

The table is written into this page by `tools/coverage.mjs --per-crate --write`, between the two
marker comments, rather than printed to a terminal for somebody to paste. It listed 25 crates
against the workspace's 29: three are excluded from the run by name and the exclusion is stated
below, and the fourth is `inillucent`, the facade, whose body is a re-export and which therefore
emits no regions at all. `cargo test -p inillucent-compat --test documentation` fails when a
workspace member is neither in the table, nor in `tools/coverage.mjs`'s `EXCLUDED`, nor named in a
sentence here.

Three rows need reading rather than ranking.

`inillucent-driver-capi` reads 0.8%, and the C ABI is not untested: its
conformance suite drives the symbols through a C program that links the built
`cdylib`, which is a separate binary from the instrumented test executables this
measurement merges. What the number says is that no Rust test calls those
functions, which is true and is what a C ABI is for.

`inillucent-compat` and `inillucent-cli` are the two crates that are mostly
*programs*: eighteen gate and profiling binaries between them, each run by hand
or by a scheduled job rather than by `cargo test`. The library halves of both
are covered by the suites that use them. Their percentages are in the table
above and are not repeated here. They were repeated here, in prose eleven lines
under the table, and the two copies disagreed in the first decimal place -
which is what a number written twice does.

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
# the open.prepare family on its own, without a whole scorecard. It takes the
# SQLite fixture and imports its own copy for the native arms, so one file is
# all it is given.
target/release/inillucent-prepareperf <dir>/medium-prepare.db 30

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
