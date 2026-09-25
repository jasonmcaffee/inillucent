# AGENTS.md: working with inillucent

This file is for an AI agent. It has two parts, and you need only one of them:

- **You are using inillucent.** You are putting a database in an application, querying one, or
  migrating data into one. Read [section 1](#1-using-inillucent) and stop. The skills in
  [`agent-skills/`](agent-skills/README.md) have one page per job.
- **You are changing inillucent.** You are editing this repository. Read all of this file. Five
  rules here are checked by tests, and a guess at any of them fails the build.

Words such as B-tree, page, WAL, pragma and HNSW are explained in [the glossary](docs/glossary.md),
one sentence each.

---

## 1. Using inillucent

inillucent is an embedded SQL database written in Rust. One `.rdb` file holds two engines:

- a **relational engine** that speaks SQLite's dialect on its own storage;
- a **retrieval engine** for vector search (HNSW) and keyword search (BM25), reachable from the same
  SQL.

A table and a search index in the same file commit and roll back together. inillucent links no other
database engine.

An install gives you four programs:

| Program | What it is |
|---|---|
| `inillucent` | the command line: 30 commands, each with `--output json` |
| `inillucent-shell` | an interactive shell that works like `sqlite3`, with 63 of its 65 dot commands |
| `inillucent-mcp` | the same commands served to an AI agent over MCP |
| `inillucent-migrate` | builds a database from a legacy retrieval index; `inillucent migrate` covers SQLite files and PostgreSQL or MySQL servers |

```sh
inillucent create app.rdb
inillucent --db app.rdb exec "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)"
inillucent --db app.rdb exec "INSERT INTO note (body) VALUES (?1)" --params '["hello"]'
inillucent --db app.rdb query "SELECT * FROM note" --output json
inillucent help                 # every command
inillucent help migrate         # one command and every parameter it takes
```

### Four rules for using it

1. **Exit code 3 means the engine has not built that feature.** Exit code 1 means a real failure.
   The two codes are separate so a script can tell "not built yet" from "wrong" without reading the
   message. Over a language binding or MCP, exit code 3 is the status `unsupported`. Rewording the
   SQL does not help. Use a different construct, or check `inillucent capabilities` first.
2. **Ask `inillucent capabilities` before you write an unusual statement.** It lists what the engine
   can do. In that table, every row but two is checked against the running engine by a test, in
   both directions: a row that says yes and fails, or a row that says no and works, fails the
   build. The two rows with no check are `cancel` and `readonly_open`. Both are `partial`. `cancel` is about when a
   running statement stops, and `readonly_open` is about which layer refuses a write. Neither can be
   tested by running one statement, and each row's own note says so.
   `cargo test -p inillucent-driver --test capability` fails if a third unchecked row is added
   without this paragraph changing.
3. **Use `--output json` when a program reads the result.** Every command accepts it. The JSON
   object is the one a language binding sees: typed values, a `total` that counts every row even
   when `--limit` cut the list, and the driver's status name on a failure. Parse the JSON, not the
   text table.
4. **Bind values with `--params`.** `--params '["a", 2]'` binds `?1`, `?2` and so on, in order.
   Pasting values into the SQL text causes quoting bugs in every language.

### Where to find answers

| Question | Page |
|---|---|
| Does a SQL feature work? | [`docs/sql.md`](docs/sql.md), and [`docs/feature-comparison.md`](docs/feature-comparison.md) for the 416 measured cases |
| How fast is it next to SQLite? | [`docs/performance.md`](docs/performance.md) |
| How does its search compare with pgvector? | [`docs/retrieval-quality.md`](docs/retrieval-quality.md) |
| How do I search by meaning or by keyword? | [`docs/vector-search.md`](docs/vector-search.md) |
| How do I use it from Python, Node, Go, PHP or C? | [`drivers/README.md`](drivers/README.md) |
| How does the retrieval engine work? | [`docs/architecture.md`](docs/architecture.md) |
| How does the SQL engine work? | [`docs/relational-architecture.md`](docs/relational-architecture.md) |
| How do the two engines fit together? | [`docs/architecture-overview.md`](docs/architecture-overview.md) |
| What is not built yet? | [`docs/roadmap.md`](docs/roadmap.md) |
| What does a term mean? | [`docs/glossary.md`](docs/glossary.md) |
| Where is every page? | [`docs/README.md`](docs/README.md) |

---

## 2. Changing inillucent

### The five rules a test checks

Read the file for a rule before you write the code it covers. Each file is short.

| Rule | Where it is written | The test that fails |
|---|---|---|
| **Dependencies**: only crates on an allowed list | `docs/dependency-policy.md`, `docs/invariants/layering.toml` | `cargo test -p inillucent-compat --test tooling policy::` |
| **Layering**: which crate may depend on which | `docs/invariants/layering.toml` | `cargo test -p inillucent-compat --test tooling harness::`, `the_workspace_obeys_the_dependency_contract` |
| **Test selection**: every test target has a row | `tests/selection.toml` | `cargo test -p inillucent-compat --test tooling selection::`; `no_test_hides_outside_the_map` names the target |
| **One command table**: the command line and MCP are generated from it | `crates/inillucent-cli/src/command/registry.rs` | `cargo test -p inillucent-compat --test tooling command_parity::` |
| **The testing standard**: where a new test goes and how the suite runs | [`tests/inillucent-testing-tdd.md`](tests/inillucent-testing-tdd.md) | none; a reviewer checks it |

### Adding a dependency

You probably cannot add one. Production crates may not link another database engine, SQL parser,
storage engine, B-tree or LSM library, transaction manager, write ahead log or query optimizer. The
check matches on substrings, so renaming a crate does not get it through.

Infrastructure crates are allowed: error handling, operating system calls, numeric kernels. Each one
needs an `[[external]]` row in `docs/invariants/layering.toml` that names every crate allowed to use
it.

`docs/dependency-policy.md` has two worked examples. One is a dependency that was added. The other
is a PostgreSQL and MySQL client that was written in `crates/inillucent-remote` instead of taken
from crates.io, with the reasons.

### Running the tests

Do not run `cargo test --workspace` while you work. Use the parallel runner, which runs only the
tests your change can affect:

```sh
cargo build -p inillucent-compat --bin inillucent-testrun --features testrun

target/debug/inillucent-testrun --tier smoke            # the smallest tier, while editing
target/debug/inillucent-testrun --changed               # what your uncommitted edits can break
target/debug/inillucent-testrun --changed origin/main   # the same, after you have committed
target/debug/inillucent-testrun --changed --list        # the selection, without running it
target/debug/inillucent-testrun                         # every tier except nightly
target/debug/inillucent-testrun --cadence nightly       # every tier
target/debug/inillucent-testrun --strict                # fail when a prerequisite is missing
```

A git worktree's `.cargo/config.toml` may point cargo at a different target directory. The runner
binary is in that directory's `debug/` folder.

`--changed` compares against `HEAD` by default. Once your work is committed, `--changed` with no
revision selects nothing and exits 0. Pass `--changed origin/main`.

**Every tier has a cadence, and `--changed` honours it.** `tests/selection.toml` gives each tier
`cadence = "change"`, `"merge"` or `"nightly"`:

| Cadence | Tiers | Where it runs |
|---|---|---|
| `change` | smoke, unit, engine, differential, e2e, retrieval, tooling | `--changed`, by the dependency closure of what you edited |
| `merge` | durability, perf | `--changed` only when a crate you edited is in the target's `covers`; every push in CI |
| `nightly` | nightly | never on a change; the nightly job, or `--tier nightly` by name |

So a parser change does not run the crash suites or the nightly stories, and a change to the write
ahead log does run `wal_crash`. A durability row's `covers` names every storage crate the suite
exercises for this reason. `--cadence merge` with `--changed` selects the merge tiers by the closure
as well, which is what CI does on a pull request.

**The build names only what was selected.** The runner builds with `-p`, `--lib`, `--test` and
`--bin` for the selected targets instead of `--workspace`. A run that selects no `inillucent-bench`
row does not compile it, so `ort`, `tokenizers` and oniguruma are not built. The runner builds
`inillucent-cli` and `inillucent-driver-capi` as programs only when a selected suite starts one.

**`inillucent-compat`'s integration tests are one binary per tier.** `tests/engine/main.rs` declares
`mod new_engine_log_lead;` and the suite is `tests/engine/new_engine_log_lead.rs`. The target is
`inillucent-compat::engine::new_engine_log_lead`, and the runner still starts it in a process of its
own, with `--exact` and only that module's test names. Under a plain cargo:

```sh
cargo test -p inillucent-compat --test tooling policy::
```

runs the `policy` suite inside the `tooling` binary. Prefer the runner: a plain `cargo test --test
tooling` runs every tooling suite in one process.

**The exit code is the result.** Read it, not the last line of output:

| Exit code | Meaning |
|---|---|
| `0` | every selected target ran and passed |
| `1` | the run happened and failed: a target failed, a target could not be read, or `--strict` found a missing prerequisite |
| `2` | **the run did not happen.** The build failed, a named selection matched nothing, `--filter` matched no test, or cargo could not report what it built. Nothing was graded. Do not read exit code 2 as a pass |

In a shell, `$?` after a pipeline is the status of the last command in the pipeline.
`inillucent-testrun --changed | tail -40` reports the exit code of `tail`, which is always 0.
Redirect to a file instead:

```sh
target/debug/inillucent-testrun --changed > run.log 2>&1; echo $?
```

**`--strict` turns a missing prerequisite into a failure.** Some suites need something the
workspace cannot build: the pinned SQLite reference build, a fixture corpus, a live PostgreSQL. Such
a suite reports success when its prerequisite is absent. `--strict` counts those suites and names
them. Setting `INILLUCENT_STRICT=1` in your shell does not do the same thing, because the runner
overwrites that variable in every child process from its own `--strict` flag.

**A machine declares what it will never have.** The development machine has no MySQL, no live
PostgreSQL and no Go, so `--strict` could not pass there. The gitignored
`tests/prerequisites.local.toml` holds `absent = ["mysql", "postgres", "go"]`, and `--absent <name>`
adds a name for one run. A strict run reports a suite whose row `requires` a declared name under
"not evidenced on this machine, by declaration", and does not fail for it. A skip for anything else
still fails. A declared name that no row requires is refused, so a misspelling cannot excuse nothing
quietly. CI passes `--absent` for what a runner cannot have, per operating system, in
`.github/workflows/tests.yml`. `--summary <file>` writes the verdict, the failures and the declared
absences as JSON.

**When the runner stops a target.** The runner stops a target only when both of these are true:

- the target has run for longer than its budget. The budget is eight times the target's recorded
  time in `tests/timings.toml`, and never less than two hours.
- the target has printed nothing for ten minutes.

A stopped target is reported as `UNKNOWN` under `STOPPED` with both numbers, is not retried, and the
run exits 1. `--timeout <secs>` replaces the budget, and `--timeout 0` removes it. Do not shorten the
budget. `inillucent::story_ledger_day_nightly` has no row in `tests/timings.toml`, takes 1800.37 s,
and prints nothing until it ends.

**The runner builds the programs once.** It builds `inillucent`, `inillucent-shell` and
`inillucent-mcp` before any suite starts and sets `INILLUCENT_PROGRAMS_BUILT`. Tests find the
programs through `cliproc::program`. Under a plain `cargo test`, `cliproc::program` builds them once
per test process, and a failed build fails the test with cargo's output.

**A nested runner starts no cargo.** The runner writes the executables it located to
`<target>/inillucent-testrun/artifacts.json` and names the file in `INILLUCENT_TESTRUN_ARTIFACTS`.
`inillucent-testrun --artifacts <file>` runs from that list. `gates_fail_closed` runs its nested
runners this way, so it no longer builds a second workspace or runs alone at the end.

**The runner sets up the MSVC compiler environment.** The `onig_sys` crate compiles C code with
`cl.exe`, which needs the `INCLUDE` variable that only a Visual Studio developer shell sets. The
runner finds Visual Studio with `vswhere`, runs `vcvars64.bat`, and passes the result to cargo. It
does nothing when `INCLUDE` is already set. Since the build names only the selected targets, a run
that does not select `inillucent-bench` does not compile oniguruma and does not need it. Other
scripts in `packaging/` still need `Import-MsvcEnvironment` from `packaging/stage-layout.ps1` loaded
by hand.

**Two optional machine settings, both off here.** `pwsh packaging/setup-machine.ps1 -Linker` sets
`CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER` to the toolchain's `rust-lld.exe`, and `-Sccache`
sets `RUSTC_WRAPPER` to sccache with its cache on D:. Both are user environment variables, because
a committed `.cargo/config.toml` would be replaced in a ticket's worktree. Measured on this machine
after the test binaries were grouped by tier, neither made a cold test compile faster, so neither is
set. `-Remove` takes them out.

### What a git worktree does not have

Two gitignored folders are missing from a new worktree. Without them some suites skip, report
success, and hide real failures. Run with `--strict` to see them.

| Folder | What needs it | How to get it |
|---|---|---|
| `.sqlite-ref/` | every suite graded against the pinned SQLite, including `semantics.rs` and everything using `differential::compare` | copy it from the main checkout, or run `pwsh tools/sqlite-reference.ps1` |
| `_agent_output/fixtures/` | `small.db`, `medium.db` and `large.db`, used by `inillucent-compat::engine::new_engine_log_lead` and `inillucent-compat::tooling::gates_fail_closed` | copy it from the main checkout, or run `tools/build-gate-fixtures.sh` |

`new_engine_log_lead` builds an index larger than the buffer pool and reopens it. That tests the
engine's open and recovery path under real memory pressure. Treat a failure there as a real defect.

### Writing a test

The standard is [`tests/inillucent-testing-tdd.md`](tests/inillucent-testing-tdd.md) and its six
rules. The two broken most often:

- **A test asserts a value.** `assert!(result.is_ok())` on a migration that published nothing passes
  and proves nothing.
- **A test must be able to fail.** A benchmark that leaves out the change under test, a check whose
  prerequisite is missing, or a limit set so wide nothing crosses it all report success and mean
  nothing.

A new `tests/*.rs` file needs a row in `tests/selection.toml`, or the `selection` suite fails and
names it. In `inillucent-compat` a new suite is a file in `tests/<tier>/`, a `mod` line in that
tier's `main.rs`, and a row with `name = "<tier>"` and `module = "<file>"`.

### House style

Read three neighbouring files before you write a new one. Then follow these rules:

- **Every function has a doc comment that says what it is for**, with `@param` lines. The governed
  crates set `deny(missing_docs)`, and a test checks that every module states its invariant.
- **A comment explains why.** Say why the obvious approach is wrong, what was measured, what failed
  before, or what another choice would cost. The comment in `crates/inillucent-remote/src/migrate.rs`
  that explains why the digest is a sum and not an exclusive or is a good model.
- **A comment claims only what its test proves.** This is rule 1.5 of the testing standard. A
  reviewer checks it.
- **No `unwrap`, `expect`, `panic!` or slice indexing** on any code path that reads SQL text,
  database pages, journal frames, network bytes or file system results. The governed crates deny all
  four and allow them only under `#[cfg(test)]`.
- **Run `cargo fmt` before you finish.** `policy.rs` fails on an unformatted governed crate.
- **Documentation follows [the writing style guide](docs/writing-style.md).** Short sentences, plain
  words, no dashes used as punctuation, no hyphenated compounds in prose.
- **Never put a ticket number in a published document.** A key such as `task-NNNN` names a card on a
  private board that readers cannot open. This covers `README.md`, `CHANGELOG.md`, this file,
  `CLAUDE.md`, everything under `docs/`, `agent-skills/` and `packaging/`, and the driver and example
  readmes. Say what the change did, or give a commit hash or a date. A path to a design document
  under `tasks/` is a file name and may stay. `node tools/doc-facts/check.mjs` fails on any other
  ticket number. Commit messages, code comments and the design documents under `tasks/` are not
  covered.

### What a finished change includes

1. The code, with doc comments that explain why.
2. Its tests, placed where section 2.1 of the testing standard says, each registered in
   `tests/selection.toml`.
3. `target/debug/inillucent-testrun --changed` exits 0.
4. `cargo fmt` has run.
5. Every rule file the change touches is updated in the same commit: a layering row, a dependency
   argument, a command table entry. Each has a test that fails on the next person's machine if it is
   left out.
6. Every page that describes the behavior you changed is updated in the same change:
   - the pages under `docs/`;
   - the skills under `agent-skills/`, then `node tools/sync-skills.mjs` to copy them into
     `.claude/skills` and `.agents/skills`;
   - the package readmes under `packages/`;
   - `drivers/README.md`;
   - the chapter in `src/data/documentation.ts` in `sites/inillucent` of the `black-rainbow-labs-sites` repository, which is
     published at https://inillucent.com/docs.
7. `node tools/doc-style/check.mjs` reports no problems, and
   `cargo test -p inillucent-compat --test tooling documentation::` passes.

---

## 3. Releasing

One command publishes a release:

```powershell
pwsh packaging/ship.ps1 -WhatIf                # print the plan and change nothing
pwsh packaging/ship.ps1 -Part patch            # a full release
pwsh packaging/ship.ps1 -Only site,github      # some routes only, with no rebuild
```

### Run it from its own worktree

`ship.ps1` refuses a checkout with uncommitted changes, and so does `cargo publish`. The checkout you
work in usually has uncommitted changes, so make a worktree for the release:

```powershell
git worktree add -b release-0.1.6 J:/build/release main
pwsh -WorkingDirectory J:/build/release -File J:/build/release/packaging/ship.ps1 -Part patch
```

Pass `-WorkingDirectory`. Cargo reads `.cargo/config.toml` from the working directory, not from the
manifest it builds. A ticket's worktree has a `.cargo/config.toml` that points the target directory
at that ticket's build folder. A release started from there builds into the wrong folder, which is
what happened on the first attempt at 0.1.8.

`ship.ps1` finds the site checkout, the Homebrew tap and `tools/cross/bin` through the repository the
worktree belongs to, so a worktree on another drive needs no arguments. `-SitePath` and `-TapPath`
override them.

### The five phases

| Phase | What it does |
|---|---|
| **preflight** | reads every credential, decides which routes can run, prints the plan, and changes nothing |
| **version** | writes the new version into every file that carries it and refreshes `Cargo.lock` |
| **build** | compiles, signs and notarises. Nothing has left the machine yet |
| **publish** | tags, pushes, and sends the release to every destination |
| **report** | asks each destination what it now serves, instead of trusting an exit code |

The order matters because a tag is the one step that cannot be quietly undone. Everything that can
fail without a trace runs before it.

### The nightly, and the tests a release relies on

A release runs no full suite of its own. `packaging/nightly.ps1` runs every night at 02:00 as the
scheduled task `inillucent nightly`, which `pwsh packaging/register-nightly.ps1` registers. It works
in its own worktree, `J:/build/nightly`, moved to `origin/main`, and:

1. runs `inillucent-testrun --cadence nightly --strict --record`, with this machine's declared
   absences;
2. runs `release-all.ps1` for all five targets, which now builds them in parallel, each in its own
   target directory, still with fat LTO and one codegen unit;
3. runs `inillucent-fullgate`, `inillucent-writegate` and `inillucent-scorecard` on the medium
   fixture, built with the release profile;
4. replaces the rolling `nightly` pre release on the public mirror. It never publishes to a registry;
5. commits `tests/timings.toml`, `tests/nightly-history.tsv` and `compat/perf/nightly/` to `main`;
6. writes `_agent_output/nightly/latest.json` in the main checkout: the commit, the date, the result
   of each step, and the declared absences;
7. files a ticket on the board when the night is red, unless one for the same failures is open.

`pwsh packaging/nightly.ps1 -WhatIf` prints the plan. The tests phase of `ship.ps1` reads
`latest.json`:

| `latest.json` | `ship.ps1` |
|---|---|
| green, for the commit being released | runs no suite; the notes say "Verified by the nightly run of `<date>` at `<commit>`" |
| green, for an older commit | runs `inillucent-testrun --changed <that commit> --cadence merge --strict` and names both commits |
| red, or missing | refuses, unless `-SkipTests` |

`packaging/tests/ship-evidence.Tests.ps1` holds those three cases. Run it with `Invoke-Pester
packaging/tests`.

### The routes

`ship.ps1` publishes through fourteen routes, named as the plan prints them:

| Route | What it publishes |
|---|---|
| `build` | the five targets: Windows x86-64, Linux x86-64 and aarch64, macOS aarch64 and x86-64 |
| `linux-packages` | the `.deb` and the `.rpm` |
| `signature` | the minisign signature over `SHA256SUMS` |
| `tag` | the commit, the tag and the push |
| `mirror` | the public source mirror on GitHub |
| `github` | the GitHub release with every asset |
| `interop` | `tests/interop/<version>`, built by the binary this release publishes |
| `site` | inillucent.com: the files, then the links |
| `crates` | crates.io |
| `npm` | the npm wrapper and its four platform packages |
| `pypi` | the Python wheel |
| `go` | the Go module tag |
| `packagist` | Composer, which reads the tags again |
| `homebrew` | the Homebrew formula in the tap |

A route with no credential is skipped, and the skip message says how to fix it. The script does not
fail for a missing credential, because a script that needs all of them would never be run.

Credentials are sealed with Windows DPAPI under `%LOCALAPPDATA%\inillucent\signing` and unsealed to a
RAM disk for the run. That covers the Apple Developer ID and notary key, minisign, OpenPGP, and the
npm, PyPI, Packagist and crates.io tokens. Nothing has to be exported by hand. GitHub needs nothing
either: the token comes from the credential `git push` already uses.

### Do not run the other scripts in `packaging/` by hand

`ship.ps1` calls them in order. Running them one at a time left 0.1.3 tagged and half published for
four days, with a GitHub release still in draft. Uploads to a draft release report success while
nothing is visible.

### This machine builds every target

No Mac and no Linux machine is needed. zig links the macOS binaries, `rcodesign` signs and staples
them, and Apple's notary service is an HTTPS API. The toolchain is in `tools/cross/bin`, which is
gitignored. `pwsh tools/cross/fetch-toolchain.ps1` downloads it, and a worktree shares the main
checkout's copy. The Linux archives, the `.deb` and the `.rpm` are built here too.

A fix to anything a release ships (a binary, the `.pkg`, an installer script, a wrapper package) is
finished when `ship.ps1` has published it and every route reports it. Merging the fix is not enough,
and "there is no Mac here" is never a reason to stop. The one thing this machine cannot do is run
Installer.app or a macOS binary. If that matters, say so in those words after the release is out.

### Five release mistakes to avoid

- **A registry records a version's commit the first time it sees the tag, and never changes it.**
  proxy.golang.org and Packagist both do this. `gh release create` against a tag the mirror does not
  have yet creates the tag at the mirror's current HEAD, which is the previous release. Both
  registries then serve the previous release's source under the new version, permanently. So
  `mirror` runs before `github`, and `packagist` runs after both. inillucent's Go module at v0.1.5 and
  Composer package at v0.1.6 are wrong for this reason.
- **The version is written in eight files.** `Get-VersionCarriers` in `ship.ps1` lists them. If you
  add a file that carries the version, add it there. Two of the eight carry the version in code: the
  PHP installer's `NATIVE_VERSION` and the Go wrapper's `nativeVersion`. The scan for leftover
  version numbers cannot see the Go one, because `packages/go/` names old versions in test data.
- **A published version is permanent.** npm, crates.io and PyPI refuse to replace one, and npm never
  reuses a version number that was unpublished. `inillucent@0.1.3` and `@0.1.4` on npm are
  deprecated because they shipped broken.
- **`packaging/install.sh` must use LF line endings and contain no carriage return.** `sh` on Debian
  and Ubuntu is dash, which reads a carriage return as part of the command and fails with
  `set: Illegal option -`. The `site` route refuses a shell script that does not parse or that holds
  a carriage return.
- **The Windows build needs the MSVC environment.** `onig_sys` compiles C code with `cl.exe`, and an
  agent terminal has no `INCLUDE`, so the build fails on `stddef.h`. `Import-MsvcEnvironment` runs
  `vcvars64.bat` when it is needed.

### Check what was published

`Test-SiteVersion` downloads every file named in the published `SHA256SUMS`. Each registry route asks
its registry what it serves, and retries for three minutes because registries take time to update.
Running the real installs once found four defects that had all reported success: a `curl | sh`
script that did not parse, one PyPI wheel where there should have been four, a Homebrew formula that
was written and never pushed, and a mirror route that had never pushed anything.
