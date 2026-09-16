---
name: inillucent-develop
description: Work on the inillucent repository itself - the five contracts a test enforces, the parallel test runner, the one command table behind the CLI and MCP, where a new test goes, and the house style. Use when changing, extending, reviewing or debugging code in this repo, adding a dependency, adding a command, or adding a test.
---

# Working on inillucent

Five contracts here are enforced by tests. Guessing at any of them produces a red build rather than a
review comment, and each is short enough to read before you write the code.

| contract | where | what fails |
|---|---|---|
| dependencies | `docs/dependency-policy.md`, `docs/invariants/layering.toml` | `--test policy` |
| layering | `docs/invariants/layering.toml` | `--test policy`, `the_workspace_obeys_the_dependency_contract` |
| test selection | `tests/selection.toml` | `--test selection` |
| one command table | `crates/inillucent-cli/src/command/registry.rs` | `--test command_parity` |
| the testing standard | `tests/inillucent-testing-tdd.md` | — (review) |

## Running the tests

**Not `cargo test --workspace` while you iterate.** There is a selective parallel runner:

```sh
cargo build -p inillucent-compat --bin inillucent-testrun --features testrun

target/debug/inillucent-testrun --tier smoke        # ~1 s, mid-edit
target/debug/inillucent-testrun --changed           # what your edits can break
target/debug/inillucent-testrun --changed --list    # ...without running it
target/debug/inillucent-testrun --changed origin/main
target/debug/inillucent-testrun                     # everything, ~300 s
target/debug/inillucent-testrun --strict            # fail on a missing prerequisite
```

Selection reads `git diff --name-only` **plus** `git ls-files --others`, so a brand new file counts;
maps each path to a package; closes over **reverse** dependencies; then picks the targets whose
`covers` names one of them. A path matching no `[[path]]` rule selects **everything** — the safe
direction, and what makes a new top-level directory loud rather than invisible.

**`--strict` is not optional when you are about to claim a result.** Several suites need something
the workspace cannot build — the pinned SQLite oracle, a fixture corpus, a live PostgreSQL — and each
*reports success* when it is absent. `--strict` counts and names those, so a green on a bare machine
cannot be mistaken for a green.

## Adding a dependency

**You probably cannot.** Production crates may not link another database engine, SQL parser, storage
engine, B-tree or LSM library, transaction manager, WAL or query optimizer; the check matches
substrings so a rename does not slip past. What is allowed is infrastructure — error plumbing, the
OS boundary, numeric kernels — with an `[[external]]` row naming the crate, its category and every
first-party crate that may use it, plus a sentence in `docs/dependency-policy.md`.

Read the two worked arguments there first: the deliberate additions, and **the dependency that was deliberately
not add**. That one is the more useful model — a PostgreSQL and MySQL client, written first-party in
`crates/inillucent-remote` rather than pulled in, with the reasons and the limits it accepts.

```sh
cargo run -p inillucent-compat --bin inillucent-manifest -- layering
```

## Adding a command

There is **one table**: `crates/inillucent-cli/src/command/registry.rs`. `inillucent --help`, the
argument parser, `inillucent-mcp`'s `tools/list` and every JSON Schema it publishes are all derived
from it, and `command_parity.rs` fails the build if any of them stops agreeing. So:

1. Add a `Command` row and its `Param` list to `registry.rs`.
2. Write `pub fn <verb>(context, arguments) -> Result<Outcome, Failed>` in `verbs.rs`.
3. Nothing else. The MCP tool and the help text appear on their own.

Write each description **for two readers at once**: a person running `inillucent help <verb>`, and a
model reading it as a tool description with one attempt at getting the call right. Say what a
parameter is *for* rather than restating its name, and say so explicitly where there is a trap —
`limit` cutting the rows handed back but not the count, `params` being positional.

Two things the surface already decides for you, and which a new verb must not re-decide:

- **`Failed::from_engine`** classifies an engine error. There is exactly one place in this repository
  that decides whether a refusal is `unsupported` or `syntax`; a second copy drifts the first time the
  engine grows a construct.
- **`context.confine(path)`** for any path, and **`context.confined()`** for anything that can reach
  something *other* than a path — a host, a port, a URL. `--root` is about reach, not only paths.
- **The decision is not in the CLI.** `context.confine` goes through
  `inillucent_vfs::confine`, which is a *module* rather than a function: `Root::admit` and
  `Root::admit_path` resolve a path against the root, and `confine::authorize` is what `OsVfs`
  calls again before it opens, deletes or stats anything. The CLI's copy exists to name the path a person typed in the
  refusal; it is not a second policy, and a new file operation must not grow one. If code you are
  writing opens a file from a path a caller supplied, it is already confined — do not add a check,
  and do not reach past `OsVfs` to `std::fs`.

## Adding a test

`tests/inillucent-testing-tdd.md` §2.1 says where it goes:

| testing | goes |
|---|---|
| one function or module | `#[cfg(test)]` in the crate — tier `unit` |
| a construct SQLite also has | `inillucent-compat/tests/`, graded against the oracle — `differential` |
| SQL or storage with no SQLite equivalent | `inillucent-compat/tests/` — `engine` |
| what an application does with the public API | `crates/inillucent/tests/` — `e2e` |
| what survives a crash or injected fault | `inillucent-compat/tests/` under the simulator — `durability` |
| a cost that must not change shape | `crates/inillucent/tests/budget.rs` — `perf` |

**A new `tests/*.rs` file needs a row in `tests/selection.toml`**, or `selection.rs` fails and names
your target. If the suite needs something the workspace cannot build, add it to that row's `requires`
and make the suite print one of the phrases the runner recognises — `is not built`, `is missing`,
`; skipping` — so `--strict` can tell a real pass from an empty one.

The two rules broken most often:

- **A test asserts a value, not the absence of a crash.** `assert!(result.is_ok())` on a migration
  that published nothing is a passing test of nothing.
- **A test that cannot fail is worse than no test.** A benchmark that excludes the change under test,
  a gate whose bound is straddled, a suite whose prerequisite is silently absent — each reports green
  and means nothing.

## House style

Read three neighbouring files before writing one. The conventions that carry weight:

- **Every function carries a doc comment saying what it is for**, with `@param` lines. Governed
  crates `deny(missing_docs)`, and `policy.rs` checks that every module states its invariant.
- **Comments carry the argument, not the mechanics.** The valuable comment here says *why the obvious
  thing is wrong*: what was measured, what failed before, what the alternative would cost.
  `// increment the counter` is noise. The paragraph in `crates/inillucent-remote/src/migrate.rs`
  explaining why the digest is a sum and not an exclusive-or is the style.
- **A comment may only claim what its test proves** (testing standard, rule 1.5).
- **No `unwrap`, `expect`, `panic!` or slice indexing** on any path reading SQL text, database pages,
  journal frames, network bytes or VFS results — the governed crates `deny` all four and relax them
  only under `#[cfg(test)]`. `saturating_add`, `.get(..)` and `let … else` are the idioms in use.
- **`forbid(unsafe_code)`** unless the crate is on `policy.rs`'s allow-list with a SAFETY note per
  call.
- `cargo fmt` before finishing; `policy.rs` fails on an unformatted governed crate.

## Before you say it is done

1. `target/debug/inillucent-testrun --changed`, green.
2. `cargo fmt`.
3. Every contract file the change touches, updated **in the same commit** — a layering row, a
   dependency argument, a command-table entry, a `selection.toml` row. Each is checked by a test that
   will otherwise fail on somebody else's machine.
4. If you measured something, the number and how it was taken. If you did not, do not imply you did.
