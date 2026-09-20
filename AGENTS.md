# AGENTS.md — working with inillucent, for an AI agent

> ## Releasing: one command
>
> ```powershell
> pwsh packaging/ship.ps1 -WhatIf    # the plan, and nothing written
> pwsh packaging/ship.ps1 -Part patch
> pwsh packaging/ship.ps1 -Only site,github    # part of a release, no rebuild
> ```
>
> **Run it from a `git worktree`, not from the checkout you work in.** It refuses a dirty tree and
> so does `cargo publish`, and the ordinary checkout usually has something in flight:
>
> ```powershell
> git worktree add -b release-0.1.6 J:/build/release main
> pwsh J:/build/release/packaging/ship.ps1 -Part patch
> ```
>
> It finds the site checkout, the Homebrew tap and `tools/cross/bin` through the repository the
> worktree belongs to, so a worktree on another drive needs no arguments. `-SitePath` and `-TapPath`
> override them.
>
> ### What it publishes
>
> Twelve routes: the five build targets, the Linux packages, the signature over `SHA256SUMS`, the
> tag, the GitHub release, the public mirror, inillucent.com, **crates.io, npm, PyPI, the Go module
> tag and the Homebrew tap**. A route with no credential is a **skip carrying the sentence that
> fixes it**, never a failure - a script that refuses without all twelve is one nobody runs.
>
> Every credential is DPAPI-sealed under `%LOCALAPPDATA%\inillucent\signing` and unsealed to the
> RAM disk for the run: the Apple Developer ID and notary key, minisign, OpenPGP, and the npm,
> PyPI and crates.io tokens. **Nothing needs to be exported by hand.** GitHub is the exception and
> needs nothing either - the token comes from the credential `git push` already uses.
>
> ### The five phases, and why the order is the design
>
> **preflight** reads every credential and prints the plan while mutating nothing, because a tag is
> the one step that cannot be taken back quietly. **version** writes the new version into all seven
> files that carry it and refreshes `Cargo.lock`. **build** compiles, signs and notarises; nothing
> has left the machine yet. **publish** tags, pushes and reaches every destination. **report** asks
> each destination what it serves rather than trusting an exit code.
>
> ### Do not run the scripts in `packaging/` by hand
>
> They are what it calls. Running them one at a time is how 0.1.3 ended up tagged, half published
> and left that way for four days, with a GitHub release that was still a **draft** - uploads go
> into a draft and report success while nothing is visible.
>
> ### macOS is built here, on Windows
>
> No Mac is involved. zig cross-links the Mach-O, `rcodesign` replaces `lipo`, `codesign`,
> `productsign`, `notarytool` and `stapler`, and Apple's notary is an HTTPS API. The toolchain is in
> `tools/cross/bin`, which is gitignored - `pwsh tools/cross/fetch-toolchain.ps1` fetches it, and a
> worktree shares the main checkout's copy.
>
> ### Five things that will waste an afternoon
>
> - **A registry pins a version's commit the first time it sees the tag, and never moves it.**
>   proxy.golang.org and Packagist both do this. The GitHub release is created on the mirror, and
>   `gh release create` against a tag that does not exist makes one at that repository's current
>   HEAD - so if the mirror has not been pushed yet, both registries cache the *previous* release's
>   source under the new version and it cannot be corrected. That is why `mirror` runs before
>   `github`, and why `packagist` runs after both. inillucent's Go module at v0.1.5 and Composer
>   package at v0.1.6 are permanently wrong for this reason.
> - **The version lives in eight files.** Add a ninth and put it in `Get-VersionCarriers`. Two of
>   the eight name it in code rather than in a manifest - the PHP installer's `NATIVE_VERSION` and
>   the Go wrapper's `nativeVersion` - and the Go one cannot be caught by the straggler scan,
>   because `packages/go/` legitimately names old versions in test data.
> - **A published version is permanent.** npm, crates.io and PyPI all refuse to replace one, and an
>   unpublished npm version number can never be reused. `inillucent@0.1.3` and `@0.1.4` on npm are
>   deprecated because they shipped broken and could not be fixed in place.
> - **`packaging/install.sh` must be LF and contain no carriage return.** `sh` on Debian and Ubuntu
>   is dash, which reads a CR as part of the command and dies at `set: Illegal option -`. It shipped
>   unrunnable for three releases, because a literal CR inside a `tr -d` argument is also what made
>   git skip it when `.gitattributes` converted every other `.sh` to LF. The site route refuses to
>   publish a shell script that does not parse or that holds a CR.
> - **The Windows build needs the MSVC environment.** `onig_sys` compiles oniguruma with cl.exe, and
>   an agent terminal has no INCLUDE, so it fails on `stddef.h`. `Import-MsvcEnvironment` runs
>   vcvars64 when it has to.
>
> ### Verify against what is published, not against the build
>
> `Test-SiteVersion` fetches every name in the published `SHA256SUMS`, and each registry route asks
> that registry what it serves, retrying for three minutes because they are eventually consistent.
> Running the installs found four defects that had all reported success at release time: a
> `curl | sh` that did not parse, one PyPI wheel where there should have been four, a Homebrew
> formula written into the tap and never pushed, and a mirror route that had never pushed anything.

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
| `inillucent-shell` | an interactive shell shaped like `sqlite3`, with 63 of its 65 dot commands |
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
   row but two is checked against the running engine by a test **in both directions** — a claimed
   capability that fails and a denied one that now works each turn the build red. That makes it
   worth trusting in a way a hand-written feature list is not. The two exceptions are `cancel` and
   `readonly_open`, both `partial`: what they claim is about *when* a statement stops and *which
   layer* refuses a write, and neither is a thing one statement can be run to find out. Each says so
   in its own note, and `cargo test -p inillucent-driver --test capability` fails if a third joins
   them without this sentence changing.
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
| what does that word mean? | [`docs/glossary.md`](docs/glossary.md) — B-tree, page, WAL, pragma, rowid, collation, HNSW, BM25, one sentence each |
| how do the two engines fit together? | [`docs/architecture-overview.md`](docs/architecture-overview.md), both halves in one diagram |
| where is everything? | [`docs/README.md`](docs/README.md), the documentation index |

---

## 2. Working on inillucent

Words used below and not explained here - VFS, WAL, journal, B-tree, pragma, leaf, page - are in
[`docs/glossary.md`](docs/glossary.md), one sentence each.

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
target/debug/inillucent-testrun                   # everything, ~300 s
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
