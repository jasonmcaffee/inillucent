# Contributing

**[`AGENTS.md`](AGENTS.md) §2 is the contributor guide.** It is written for an AI
agent and it is exactly as true for a person: the five contracts a test enforces,
what a dependency is allowed to be, where a test goes, and the house style. Read
it before writing code and you will not have to undo anything.

This file adds the two things it does not say.

## Running the suite without the oracle

Most of what this repository asserts is a comparison against a pinned SQLite
3.53.4, built from the published amalgamation and verified against the SHA3-256
sum SQLite publishes. It is not checked in and it is not a dependency; it is a
child process the differential suites talk to.

```sh
pwsh tools/sqlite-reference.ps1     # Windows
sh   tools/sqlite-reference.sh      # everything else
```

Seventy-odd differential suites need it. **Without it they skip, and a skip here
is not a pass** — `target/debug/inillucent-testrun --strict` counts every suite
that ran with a prerequisite missing and fails, naming each one. A green run with
nothing installed would otherwise look exactly like a green run.

The other prerequisites, and what skips without each:

| absent | what stops running |
|---|---|
| the pinned oracle | every differential suite, about seventy |
| a built `inillucent` binary | the command-line and confinement suites |
| a PostgreSQL server | `inillucent-remote::live_postgres` |
| a MySQL server | `inillucent-remote::live_mysql` |
| an ONNX model | the embedding suites in `inillucent-core` |

The last two are expected to be absent on an ordinary machine; the rest are one
command each. `tools/validate.ps1` and `tools/validate.sh` build everything that
can be built before they grade anything, which is why they are the gate rather
than `cargo test`.

**Do not run `cargo test --workspace` while you iterate.** There is a parallel,
selective runner and it is much faster:

```sh
cargo build -p inillucent-compat --bin inillucent-testrun --features testrun

target/debug/inillucent-testrun --tier smoke     # about a second, mid-edit
target/debug/inillucent-testrun --changed        # what your edits can break
target/debug/inillucent-testrun                  # everything
target/debug/inillucent-testrun --strict         # and fail on a missing prerequisite
```

## A pull request carries its contract files

Five things in this repository are enforced by a test rather than by a reviewer,
and each of them fails a build on somebody else's machine if it is not updated in
**the same change**:

| you changed | update, in the same commit |
|---|---|
| a crate's dependencies | `docs/dependency-policy.md` and `docs/invariants/layering.toml` |
| a `tests/*.rs` file, or added one | its row in `tests/selection.toml` |
| the command line or MCP | `crates/inillucent-cli/src/command/registry.rs`, which both are generated from |
| a module or function past its recorded size | extract something; the ratchets in `crates/inillucent-compat/tests/policy.rs` only ever come down |
| anything a document states a number about | the document, which `tools/doc-facts/check.mjs` checks against the running engine |

Before opening a pull request:

```sh
pwsh tools/validate.ps1        # Windows
sh   tools/validate.sh         # everything else
```

That is the same gate CI runs. It builds the oracle and the fixtures, runs `fmt`,
`clippy -D warnings`, `cargo deny check`, `cargo doc` with warnings denied, the
four contracts, the security suites, and the whole selected suite with
`--strict`.

## What a change looks like when it is finished

1. The code, with its doc comments and the argument for why the obvious thing is
   wrong.
2. Its tests, where `tests/inillucent-testing-tdd.md` §2.1 says they go,
   registered in `tests/selection.toml`.
3. `target/debug/inillucent-testrun --changed`, green.
4. `cargo fmt`.
5. Whatever contract file the change touched, from the table above.

And one rule from the testing standard that is worth repeating here, because it
is the one most often broken: **a test asserts a value, not the absence of a
crash**, and **a test that cannot fail is worse than no test**. If you are not
sure yours can fail, revert the fix and watch it go red.

## Reporting a security problem

Not here. [`SECURITY.md`](SECURITY.md) says where.
