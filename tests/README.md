# Tests: shared files and the testing standards

This folder holds the files that more than one test crate reads: fixtures, recorded reports, the
test selection map and the history files. The tests themselves live in each crate's `tests/` folder.
A fixture lives here so that no single crate owns it.

## The documents

| Document | What it covers |
|---|---|
| [`inillucent-testing-tdd.md`](inillucent-testing-tdd.md) | the testing standard: where a new test goes, its six rules, and how the parallel runner works |
| [`synthetic-corpus.md`](synthetic-corpus.md) | how to build the public corpus that every retrieval measurement uses, from download to graded run |
| `inillucent-e2e-scenarios-tdd.md` | the design for the end to end scenarios and the harness that runs them |

## The files

| File | What it is | What reads or writes it |
|---|---|---|
| `selection.toml` | the map from a changed path to the test targets that path can break. Every test target needs a row | read by `inillucent-testrun --changed`. `crates/inillucent-compat/tests/tooling/selection.rs` fails on a target with no row |
| `timings.toml` | how long each test target took under the parallel runner, in milliseconds | written by `inillucent-testrun --record`. The runner starts the longest targets first and uses these times for its time limit |
| `performance-history.tsv` | what each benchmark workload cost, beside the pinned SQLite 3.53.4 | appended by `inillucent-perfhistory`, one row per workload per run |
| `nightly-history.tsv` | which long suite passed, and when | appended by `pwsh tools/run-nightly.ps1`, one row per target per run |
| `fuzz-history.tsv` | each fuzz run and its outcome | appended by `pwsh tools/run-fuzz.ps1`. See [`fuzz/README.md`](../fuzz/README.md) |
| `escapes.toml` | every defect that reached a user, and the test that now catches it | read by `crates/inillucent-compat/tests/tooling/escapes.rs` |

## The folders

| Folder | What it holds |
|---|---|
| `conformance/` | SQLLogicTest files whose expected answers were recorded from the pinned SQLite. An upstream `.test` file copied here runs with no new code |
| `crash/` | the reports from the failure campaigns: power loss, I/O errors, full disks and short writes at every file system call of a write. See [`crash/README.md`](crash/README.md) |
| `interop/` | a database written by each published release, with the answers that release gave. The current build must read every one. See [`interop/README.md`](interop/README.md) |
| `schedules/` | recorded orderings of two concurrent writers, with the commit order, the number of refused attempts and the integrity check result for each |
| `workloads/` | the known failures of the application workloads (`edges`, `nikaya`, `rag`), one `allow.list` each, and the statements taken from Nikaya's source. `story_edges.rs`, `story_nikaya.rs` and `story_rag.rs` in `crates/inillucent/tests/` read them |

The VFS conformance suite is in `crates/inillucent-vfs/src/conformance.rs`, not here. Three VFS
implementations run it, and the simulator must be able to run it against itself, so it is a library.

## How `conformance/select-foundational.test` is made

```sh
cargo run -p inillucent-compat --bin inillucent-slt
```

`inillucent-slt` asks the pinned SQLite 3.53.4 the questions in `compat/corpus/select/queries.sql`
and records its answers in `conformance/select-foundational.test`. The database those answers came
from is checked in as `compat/fixtures/select-corpus.db`. So the suite grades inillucent against
SQLite on a machine that has no SQLite installed.

`inillucent-slt` never runs inillucent. A file that recorded inillucent's own answers as the
expected answers could not find a difference.

## Running the tests

Use the parallel runner. [`AGENTS.md`](../AGENTS.md) section 2 lists its flags and exit codes.

```sh
cargo build -p inillucent-compat --bin inillucent-testrun --features testrun

target/debug/inillucent-testrun --tier smoke     # the smallest tier
target/debug/inillucent-testrun --changed        # the targets your uncommitted edits can break
target/debug/inillucent-testrun --list-tiers     # the tiers in selection.toml
target/debug/inillucent-testrun --strict         # fail when a prerequisite is missing
```
