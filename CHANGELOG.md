# Changelog

Every released version, what it was for, and what it is known not to do. The
dates are the dates the release was cut.

The version is the workspace's, which every package carries: the command line,
the shell, the MCP server, the migration tool, the C ABI library, and the Go,
npm, PyPI and Composer wrappers are all one number. `tools/doc-facts/check.mjs`
fails the build when any copy of it disagrees.

## 0.1.4 — 2026-09-17

**`embed(TEXT)` is `direct_only`, which is a behaviour change to a shipped
function.** A schema may no longer name it: a `CHECK` constraint, an index
expression, a generated column, a `DEFAULT`, a view or a trigger that calls
`embed` is refused with "may only be used from top-level SQL". A statement may
call it exactly as before.

It was registered with `FunctionFlags { deterministic: true, ..Default::default() }`,
and the `Default` derive is every flag false - so the flag said a schema may
name it while the function's own doc comment said "It stays `direct_only`: a
function that loads a 275 MB model has no business being called out of a `CHECK`
constraint or an index expression". Nothing published promised the old
behaviour: `PRAGMA function_list` does not report the bit, and no document said
a schema could call it. A `CREATE INDEX i ON t (embed(body))` would load the
model once per row of the table, inside the statement that creates the index.

`UserFunction::external` is the constructor a registrant should use for this;
`FunctionFlags::default()` exists for `builtin()`'s sake and is not what
anything registered from outside wants.

**The enforcement point is still open**, and it is a bigger gap than the flag:
`Registry::authorize_function` has no caller, so `direct_only`, `innocuous` and
`PRAGMA trusted_schema` are a policy with a passing unit test and no effect on
the engine. The flag is correct the day the binder consults it.

### The census: sixty-one places that could report success having checked nothing

The rest of this release is the task-1969 review's answer to one question - how
many places in this repository can print a green result without having checked
anything - and the answer was 61, against a page that named 5.

- **One skip helper.** `inillucent_base::testing::skipping` prints the one
  marker and panics under `INILLUCENT_STRICT`, and it is below every crate, so
  the three production crates that could not reach the test harness no longer
  print their own sentence. Twenty-four raw prints across nine files are gone,
  including a local `announce_skip` in `tests/differential.rs` that shadowed the
  library one and let nine of that file's ten tests pass on a machine with no
  SQLite oracle.
- **The map and the suites have to agree.** `tests/selection.toml` gained a
  prerequisite on seventeen rows and lost one from six that could not skip, and
  `selection.rs` now fails in both directions: a suite that can skip without a
  declared prerequisite, and a declared prerequisite whose suite cannot skip.
- **An instrument can answer about an older tree, which looks exactly like an
  answer about this one.** `tools/doc-facts/check.mjs` measured the published
  test count with `target/release/inillucent-testrun.exe`, because its binary
  lookup prefers a release build - while both validate scripts build the runner
  into `target/debug` and run it from there. The release copy on the machine
  that cut this was four days old and reported 3,010 tests where the current one
  reports 3,016. The check now takes the newer of the two and refuses one older
  than any source it was built from, naming the file and the rebuild command.
- **A contract file is only as good as the lines its parser reads.**
  `tests/selection.toml` was carrying a bare array and a repeated key, left by an
  edit that removed half a row. `toml_lite` drops the first and keeps the last of
  the second, so the file parsed, 188 rows came back and every check over it
  passed. `every_line_of_the_map_is_one_the_parser_reads` compares the file to
  itself rather than through the parser, because a line the parser drops is a
  line no other check looks at.
- **The checks outside cargo fail when they cannot check.**
  `tools/doc-facts/check.mjs` treats an instrument that cannot answer as a
  failure rather than a skip - ten of its sixteen facts were skipping on any
  fresh clone - refuses a feature-probe result recorded at another commit, and
  runs as a stage of both validate scripts and of `packaging/release.sh`.

### End to end

The layer a user touches was the layer nothing exercised. Eighteen of the thirty
command line verbs had never been passed to a spawned binary, no test had seen
exit code 3 from outside a process, no MCP tool had been called by name over
real pipes, thirty of the seventy-one dot commands appeared in no test file, and
no durability test had ever killed a real writer.

- `cli_commands.rs`: one subprocess test per verb, asserting a named field of
  parsed `--output json` or a specific exit code, and one that drives a built
  binary to exit code 3.
- `mcp_wire.rs`: one handshake, twenty-eight `tools/call` requests, one process.
- `dot_commands.rs`: every dispatched name through a real shell, and the
  63-of-65 claim held to the pinned `sqlite3`'s own list in both directions.
- `process_crash.rs`: the operating system ends a real writer twenty times and
  the file is reopened from the parent.
- `crates/inillucent-migrate/tests/cli.rs`: the migration tool as a process,
  which it had never been.
- Round trips for the npm and PHP wrappers, which had only ever read their own
  source as text, and the Python conformance runner, which `drivers/README.md`
  calls the proof that a second language can implement the driver and which was
  run by nothing.

**Two defects those tests found**, both in verbs nothing had spawned:
`inillucent --db app.rdb shell` ignored `--db` and opened `:memory:`, and
`inillucent restore <file>` accepted a backup that is not there, exited 0 and
created an empty database.

### Known not to do

- `PRAGMA trusted_schema`, `innocuous` and `direct_only` are not enforced. See
  above.
- The Go wrapper's engine tests did not run on the machine that cut this: Go
  is not installed there, and `winget install --id GoLang.Go -e` downloaded
  1.27.0, verified its hash and ended with `Installer failed with exit code:
  1603`, which is the MSI declining to install without elevation. The `wrappers`
  validate stage names the absent toolchain and the install URL rather than
  passing over it silently. The other three wrappers ran: the npm suite 8 of 8,
  both PHP suites, and the Python conformance runner's 18 cases and 69 steps.
- The retrieval baseline names 19 files under `crates/inillucent-bench` that
  moved without an amendment, so `the_retrieval_baseline_is_unchanged` fails and
  with it the `contracts`, `tests` and `doc-facts` stages of both validate
  scripts. Every other stage of `tools/validate.ps1` passes. The amendment
  belongs to whoever changed those files.

## 0.1.3 — 2026-09-15

**Tagged and not published.** The GitHub release is a draft waiting on the Linux
archives; the tag `v0.1.3` is the tree it was cut from. This entry is written
after the fact, because the release that cut it did not write one and a hole
between 0.1.2 and 0.1.4 is the kind of thing a reader assumes is a mistake in
their checkout.

What is in it is task-1962: the public Rust surface reduced to one - the facade
is a re-export of the driver rather than a second API over the same engine -
`Connection::begin`, the engine's `lib.rs` from 7,307 lines to 1,279 and
`physical.rs` from 5,708 to 297, the parameter lists that were really types,
`ImportedDatabase`'s 63 fields in six groups behind their own cells, six
roadmap items, and coverage measured at 77.2% of regions.

Seven version pins moved together, because nothing downstream can tell which is
the real one, and `tools/doc-facts/check.mjs` fails the build when any copy
disagrees.

## 0.1.2 — 2026-09-13

**`embed(TEXT)` answers in a published binary.** Every archive up to 0.1.1 was
built without `--features inillucent-cli/embed`, so
`inillucent setup-embeddings all` downloaded 620 MB of ONNX Runtime and weights
and the program that downloaded them then answered `no such function: embed`.
The feature is in the release scripts now; the published Windows and Linux
archives both answer `SELECT length(embed('hello'))` with `3072`, the Linux one
after `setup-embeddings all` on a machine that had never run it.

Cutting it found three defects in the release gate, none of them reachable by
building the workspace:

- Five release checks spoke an MCP handshake the server no longer accepts. It
  enforces the lifecycle now — `initialize` needs `protocolVersion`,
  `capabilities` and `clientInfo`, and every other method answers `-32002` until
  `notifications/initialized` arrives — and `packaging/release.ps1`'s smoke test
  refused to build the archive at all.
- Two of those read the answer through `grep -q`, which stops at its first match
  and closes the pipe; the server's next write then failed and `set -o pipefail`
  reported a pipeline that had answered correctly as failed.
- No release had ever carried an ARM Linux archive, because `rust-toolchain.toml`
  named only the two x86-64 targets.

A fourth was not about the gate: a clean checkout on Windows turned every shell
script into CRLF, because `core.autocrlf` is true and the repository carried no
`.gitattributes`. `*.sh` is pinned to LF.

## 0.1.1 — 2026-09-11

**0.1.0 is withdrawn rather than patched.** Its archives carried `README.md`,
`docs/getting-started.md` and the quickstart skill from before the Go command
was renamed, so all three told a reader to run
`go install .../packages/go/cmd/inillucent@latest` — and `@latest` resolves to a
module where that directory no longer exists. The archive the site handed out
contained an install command that failed. Replacing those archives in place
would have left two different archives both called 0.1.0, so the version was
withdrawn instead; its archives answer 404 and its GitHub release is marked
*withdrawn — use 0.1.1*.

Also in this release, each found by running a command the release ships rather
than the same command from the repository:

- `SHA256SUMS` was written with CRLF, so Linux `awk` kept the carriage return
  and `curl -fsSL .../install.sh | sh` on Ubuntu said Linux had no build and
  then listed the Linux archive on the next line.
- Two one-liners pointed at `raw.githubusercontent.com`, which answered 404.
- `install.sh` used `set -o pipefail` and `${BASH_SOURCE[0]}`, both bash-only,
  against a documented command that pipes into `sh` — which on Debian and Ubuntu
  is dash. The script is POSIX now.
- `release.ps1` could produce an empty `SHA256SUMS` and exit 0, when an inherited
  `PSModulePath` shadowed `Microsoft.PowerShell.Utility` and `Get-FileHash`
  resolved to nothing. It checks for the cmdlets it needs before doing anything.

## 0.1.0 — 2026-09-10, withdrawn

The first release: the command line, the `sqlite3`-shaped shell, the MCP server
and the migration tool, with Windows and Linux archives on inillucent.com and
the Go module published as a tag.

Withdrawn the next day for the reason above. Its archives are removed from the
site and its GitHub release keeps its assets attached, because deleting them
would remove the record of what was published.
