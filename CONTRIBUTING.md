# Contributing

**[`AGENTS.md`](AGENTS.md) section 2 is the contributor guide.** It is written for an AI agent, and
every rule in it applies to a person too. It covers the five rules a test checks, which dependencies
are allowed, where a test goes, and the house style. Read `AGENTS.md` before you write code.

This file adds three things `AGENTS.md` does not cover: the prerequisites the test suite needs, the
files a pull request must update, and the pages a change must update.

## Install the prerequisites before you run the tests

Most tests in this repository compare inillucent's answer with the answer from a pinned build of
SQLite 3.53.4. This build is called the oracle. The oracle is built from SQLite's published
amalgamation and checked against the SHA3-256 sum SQLite publishes. The oracle is not checked in and
is not a Rust dependency. The differential suites run it as a child process.

```sh
pwsh tools/sqlite-reference.ps1     # Windows
sh   tools/sqlite-reference.sh      # Linux and macOS
```

**Without the oracle, the differential suites skip and report success.** A skipped suite is not a
passed suite. `target/debug/inillucent-testrun --strict` counts every suite that ran with a missing
prerequisite, names each one, and fails.

| Missing prerequisite | What does not run |
|---|---|
| the pinned oracle | every differential suite |
| a built `inillucent` binary | the command line suites and the confinement suite |
| a PostgreSQL server | `inillucent-remote::live_postgres` |
| a MySQL server | `inillucent-remote::live_mysql` |
| an ONNX model | the embedding tests in `inillucent-core` |

An ordinary machine usually has no PostgreSQL server, no MySQL server and no ONNX model. The oracle
and the `inillucent` binary each take one command to build.

**Do not run `cargo test --workspace` while you edit.** Use the parallel runner. The runner runs only
the tests your change can affect:

```sh
cargo build -p inillucent-compat --bin inillucent-testrun --features testrun

target/debug/inillucent-testrun --tier smoke     # the smallest tier, while editing
target/debug/inillucent-testrun --changed        # what your uncommitted edits can break
target/debug/inillucent-testrun                  # everything
target/debug/inillucent-testrun --strict         # fail when a prerequisite is missing
```

## Run the gate before a pull request

```sh
pwsh tools/validate.ps1        # Windows
sh   tools/validate.sh         # Linux and macOS
```

`tools/validate.ps1` and `tools/validate.sh` are the full gate. Nothing else runs them. The gate
builds the oracle and the fixtures first, then runs:

- `cargo fmt --check`;
- `cargo clippy` with `-D warnings`;
- `cargo deny check`;
- `cargo doc` with warnings denied;
- the contract suites: `policy`, `selection`, `command_parity`, `harness` and `gates_fail_closed`;
- the security suites: `confinement`, the C ABI suites `abi` and `conformance`, and the migration
  `transport` suite;
- every selected test with `inillucent-testrun --strict`.

## Update the rule files in the same change

A test checks each of these files. If a change leaves one out, the build fails on the next person's
machine.

| You changed | Update, in the same commit |
|---|---|
| a crate's dependencies | `docs/dependency-policy.md` and `docs/invariants/layering.toml` |
| a `tests/*.rs` file, or added one | its row in `tests/selection.toml` |
| the command line or MCP | `crates/inillucent-cli/src/command/registry.rs`. Both are generated from it |
| a module past its recorded size | split the module. `no_module_grows_past_the_size_it_is_recorded_at` in `crates/inillucent-compat/tests/tooling/policy.rs` only lets a recorded size go down |
| a number that a page states | the page. `tools/doc-facts/check.mjs` compares page counts with the built programs |

## Update the documentation in the same change

A change that alters what a user sees updates every page that describes that behavior, in the same
change. These are the pages:

- the pages under `docs/`;
- the skills under `agent-skills/`, then run `node tools/sync-skills.mjs` to copy them into
  `.claude/skills` and `.agents/skills`;
- the package readmes under `packages/`;
- `drivers/README.md`;
- the chapter in `src/data/documentation.ts` in `sites/inillucent` of the `black-rainbow-labs-sites` repository, which is published
  at https://inillucent.com/docs.

Write each page by the rules in [the writing style guide](docs/writing-style.md). Two checks read
those rules:

```sh
node tools/doc-style/check.mjs                              # every page in scope
cargo test -p inillucent-compat --test tooling documentation::        # the same rules, plus links and commands
```

## What a finished change includes

1. The code, with doc comments that explain why.
2. Its tests, placed where section 2.1 of `tests/inillucent-testing-tdd.md` says, each registered in
   `tests/selection.toml`.
3. `target/debug/inillucent-testrun --changed` exits 0.
4. `cargo fmt` has run.
5. Every rule file from the table above that the change touched.
6. Every page from the list above that describes the changed behavior.

The testing standard has two rules that are broken most often. **A test asserts a value.** A check
that only proves nothing crashed is not enough. **A test must be able to fail.** A test that cannot
fail is worse than no test. To check that yours can fail, revert the fix and run the test.

## Reporting a security problem

Do not report a security problem in a public issue. [`SECURITY.md`](SECURITY.md) says where to send
it.
