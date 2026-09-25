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
> **Start it with the release worktree as the working directory**, for example
> `pwsh -WorkingDirectory J:/build/release -File J:/build/release/packaging/ship.ps1 -Part patch`.
> Cargo reads `.cargo/config.toml` from the working directory, not from the manifest it is given. An
> agent's terminal sits in its ticket's worktree, whose `.cargo/config.toml` points the target
> directory at that ticket's build folder, so a release started from there tests and builds into the
> wrong place. The 0.1.8 release's first attempt did exactly that.
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
> **This machine builds, signs, notarises and publishes every target: macOS, Linux and Windows.**
> An agent here never needs a Mac or a Linux machine to release, and must never tell Jason that it
> does. The Linux archives, `.deb` and `.rpm` are built here too. A fix to anything a release ships
> (a binary, the `.pkg`, an installer script, a wrapper package) is finished when `ship.ps1` has
> published it and every route reports it. It is not finished when the fix is merged and a release is
> recommended. The 0.1.8 `.pkg` crashed Installer.app, and the fix was merged and then left
> unreleased with "this box has no Mac" as the reason. That reason was wrong: the release was
> 0.1.9, built and notarised on this machine. The one thing this machine cannot do is run
> Installer.app or a Mac binary. If that matters, say it in those words, after the release is out.
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

**Read the exit code, and read all three of them**:

| code | what happened |
|---|---|
| `0` | every selected target ran and passed |
| `1` | the run happened and was red: a target failed, a target could not be read, or `--strict` found a suite whose prerequisite was absent |
| `2` | **the run did not happen.** The build failed, a named selection matched nothing, `--filter` matched no test, or cargo could not say what it had built. Nothing was graded, so nothing in that run may be read as a pass |

The code is the thing to branch on. `2` used to be `1`, so a build that would not compile was
indistinguishable from a real defect, and an agent read one as the other.

**A run that never ends used to be a fourth state none of those codes could describe.**
`run_one` called `Command::output()`, which reads the child's pipes to end of file rather than
waiting for the child - and a pipe reaches end of file when the last handle to its write end closes,
so anything the child started with inherited standard output holds it after the child is gone. The
runner then sat there with no child of its own, no result line for the target, no summary and no exit
code, and had to be stopped by its process id. It needed nobody to kill anything: `interchange.rs`
ran cargo through `Command::status()`, which inherits standard output, so a cargo that outlived its
test binary did it. Any child a suite starts with inherited standard output still can.

It now waits on the child. Two things follow that are worth knowing before you read a report:

- A target whose child has exited is reported, and if something it started still held the pipe open
  its status line says so. That case has no threshold in it and cannot report a working target
  wrongly.
- A target is **killed** only when it is both past a budget drawn from its own row in
  `tests/timings.toml` - eight times that time, never under two hours - **and** has printed nothing
  for ten minutes. It is then `UNKNOWN`, listed under `STOPPED` with both numbers, never retried, and
  the run exits 1. `--timeout <secs>` replaces the budget and `--timeout 0` removes it.

**Do not narrow that budget because it looks generous.** `inillucent::story_ledger_day_nightly` has
no row in `tests/timings.toml` at all - it is in the `nightly` tier, so `--record` has never seen it -
and it takes **1800.37s** while printing nothing, because libtest holds a test's own output back
until the test ends. A thirty minute floor would have killed it four tenths of a second before it
finished.

**No suite builds the programs during a run.** `cliproc::program` is the one place a test
finds `inillucent`, `inillucent-shell` or `inillucent-mcp`. The runner builds them before any suite
starts and sets `INILLUCENT_PROGRAMS_BUILT`, so `program` runs no cargo; under a plain `cargo test`
it builds once per test process and captures cargo's output. A build that fails panics with that
output. It used to be a skip, which `--strict` reported as a missing `programs` prerequisite, and a
build that had to relink while another suite ran `inillucent-shell.exe` failed on Windows with
`Access is denied. (os error 5)`. `programs` is no longer a prerequisite any row declares.

**In a shell, `$?` after a pipeline is the status of the last command in it**, so
`inillucent-testrun --changed | tail -40` reports tail's `0` however the run went. That is easy to
get wrong, and reading 60 KB of log to find out is the cost. Redirect to a file and read the
code from the runner itself:

```sh
target/debug/inillucent-testrun --changed > run.log 2>&1; echo $?
```

**`.sqlite-ref/` is what a `git worktree` does not have.** It is gitignored, so a worktree starts
without it, and every suite graded against the reference then skips silently — `semantics.rs` and
everything using `differential::compare` among them. Pass **`--strict`** to turn those skips into
named failures, which is how to tell a run that passed from a run that did not happen. Setting
`INILLUCENT_STRICT=1` in the shell does **not** do it: the runner sets that variable on every child
from its own `--strict` flag, so a value inherited from the shell is overwritten. Copy the directory
from the repository the worktree belongs to, or run `pwsh tools/sqlite-reference.ps1`.

**`_agent_output/fixtures/` is the second thing a worktree does not have.** It holds
`small.db`, `medium.db` and `large.db`, built by `tools/build-gate-fixtures.sh`, and it is gitignored
for the same reason. Without it `inillucent-compat::new_engine_log_lead` and
`inillucent-compat::gates_fail_closed` report red under `--strict` and green without it — which is
the exact confusion `--strict` exists to remove, so they read as defects in whatever was just
changed. `new_engine_log_lead` is the one to notice: its two tests build an index bigger than the
buffer pool and reopen it, which is the engine's open and recovery path under real pressure. Copy the
directory from the repository the worktree belongs to.

**The MSVC environment is now the runner's own job.** `onig_sys` compiles oniguruma with
`cl.exe`, and a terminal that is not a Developer PowerShell has no `INCLUDE`, so the whole run used
to stop at `regenc.h(39): fatal error C1083: Cannot open include file: 'stddef.h'`. The runner finds
Visual Studio through `vswhere`, runs `vcvars64.bat` and copies the result into the environment its
cargo children inherit — the same thing `Import-MsvcEnvironment` in `packaging/stage-layout.ps1`
has done for the release path, which nothing in the test path called. It does
nothing when `INCLUDE` is already set, so a developer shell is untouched, and when Visual Studio
genuinely is not installed it refuses with the sentence that fixes it rather than letting cargo
fail on a header. **Every other program in `packaging/` still needs
`Import-MsvcEnvironment` dot-sourced by hand.**

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
- **Never put a ticket number in a published document.** `task-NNNN` names a card on a private
  board, and nobody reading this repository or inillucent.com can look it up. That covers
  `README.md`, `CHANGELOG.md`, this file, `CLAUDE.md`, everything under `docs/`, `agent-skills/` and
  `packaging/`, and the driver and example readmes. Say what the change did instead, or give a
  commit hash or a date. A path to a design document under `tasks/` is a file name and may stay.
  `node tools/doc-facts/check.mjs` fails on any other ticket number in those files. Commit messages,
  code comments and the design documents under `tasks/` are not covered.

### The shape of a finished change

1. The code, with its doc comments and its arguments.
2. Its tests, where §2.1 of the testing standard says they go, registered in `tests/selection.toml`.
3. `target/debug/inillucent-testrun --changed`, green.
4. `cargo fmt`.
5. Whatever contract file the change touches — a layering row, a dependency argument, a command-table
   entry — updated in the same commit, because each of those is checked by a test that will otherwise
   fail on somebody else's machine.
