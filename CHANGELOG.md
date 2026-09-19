# Changelog

Every released version, what it was for, and what it is known not to do. The
dates are the dates the release was cut.

The version is the workspace's, which every package carries: the command line,
the shell, the MCP server, the migration tool, the C ABI library, and the Go,
npm, PyPI and Composer wrappers are all one number. `tools/doc-facts/check.mjs`
fails the build when any copy of it disagrees.

## 0.1.4 — 2026-09-17

**A statement no longer pays for the log's housekeeping on its way out, which
is worth a factor of two to four on every autocommit write.** `locking_mode =
normal` became the default in this version, and under it a connection
checkpoints and releases the file after every statement that wrote. A
checkpoint also makes the catalog's statistics honest, rolls a log segment,
writes a checkpoint record and deletes the segments it has made redundant -
work that belongs to a checkpoint somebody asked for, and that was running once
a statement. Nothing released carries the cost: 0.1.3 shipped with
`locking_mode = exclusive` as the default, under which a connection checkpoints
at close.

Measured on the medium gate against pinned SQLite 3.53.4, the same fixture and
the same disk: an autocommit insert 27.4 ms before and 8.2 ms after, an
autocommit update 22.6 ms and 8.6 ms, `schema.index` 61.5 ms and 54.2 ms. Two
thousand autocommit inserts through `inillucent-shell` took 73.4 s and now take
28.6 s, and the per-statement checkpoint is flat where it used to climb from
20 ms to 38 ms as the run went on.

What is left of an autocommit statement, timed: about 4 ms is the fold - five or
six pages written, three `fsync`s and a rollback journal created and deleted -
and about 4 ms is the statement's own execution and commit sync.

**What this costs, so it is not a surprise.** Between reclamations the log
keeps the segments that would have been deleted, up to four mebibytes - the
same bar SQLite draws at `SQLITE_DEFAULT_WAL_AUTOCHECKPOINT`, which is 1,000
pages of its 4 KiB default. **Nothing in this engine checkpoints when a
connection closes**, so that is also what a process leaves on disk when it
exits without asking for one; it was previously near zero only because the
per-statement checkpoint reclaimed every statement. Measured: 4,000 autocommit
statements against a 320 KB database leave 3.3 MB of log in one segment. A
process killed without closing leaves that same amount for the next open to
replay, where it used to leave at most one statement's worth, so the reopen
after a crash reads more and reports it. `PRAGMA wal_checkpoint`, the
`checkpoint` verb and `Database::checkpoint` all reclaim on demand.

**`Wal::retire_segments_below` was quadratic and unbounded.** It walked every
sequence number from 1, opening a file per sequence to read its header, so
every call re-asked about every segment an earlier call had already deleted.
Two thousand autocommit inserts opened 1.5 million segment headers, 1.49
million of them for a file that is not there. The sequence number is read back
from the meta record, so the cost survived a close: the same database reopened
with 2,030 segments behind it spent 19.5 ms a statement, more than half the
checkpoint, deleting nothing. It now starts at the lowest sequence that might
still be there, and deletes exactly the same files.

**`Wal::sequence_containing` answers for the segment being appended to without
reading its header off disk**, which is the answer almost every call gets.

**The staleness check a statement makes on its way in no longer opens a file.**
Before every statement a connection asks whether another process has written,
and the log half of that question cost a path lookup and a file open: it went
through `tail_on_disk`, which takes a path and walks forward from a sequence,
calling `access` at each one and opening the file to read its header. Its own
doc comment prices the check at "one `file_size` per lock acquisition", and
`Wal::tail_of_open_segment` is what makes that true - the log already holds
that segment open. Measured at 3.2 ms of an 8.8 ms autocommit statement, where
the meta-record half of the same check cost 0.03 ms. Only the open segment has
to be asked: a checkpoint is the only thing that rolls a segment, and it moves
the meta record's generation before it releases the file lock, so the
generation is seen first.

Statistics are written by a checkpoint somebody asked for - a close, `PRAGMA
wal_checkpoint`, `VACUUM`, a backup, an integrity check, a journal-mode switch -
rather than by every statement. Between those, a tree's recorded row and leaf
counts are the shape as of the last one. They were already an estimate rather
than an invariant: `PagedTree::attach` derives the leftmost leaf from the file
instead of trusting the recorded copy, and `PagedTree::check` compares the
sibling chain against the interior levels rather than against the recorded leaf
count.

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

**And the binder consults it**, which it did not until task-1972.
`Registry::authorize_function` had no caller anywhere in the workspace, so
`direct_only`, `innocuous` and `PRAGMA trusted_schema` were a policy with a
passing unit test and no effect on the engine: a `CHECK`, an index expression,
a generated column, a `DEFAULT`, a partial-index predicate, a view and a trigger
could each name any registered function whatever its flags said.

The rule itself moved down to `inillucent_sql::function::schema_refusal`, below
the binder that enforces it, and `Registry::authorize_function` calls that same
function - so an application asking the registry directly and a statement the
binder compiles cannot answer differently. `FunctionFlags` and `CallSite` moved
with it and are re-exported from `inillucent_ext::registry`, so every path an
application already writes resolves to the same type it did.

The binder carries a call site that is set at seven places: a `DEFAULT`, a
`CHECK`, a generated column's expression, an index expression, a partial-index
predicate, a view's body and a trigger's body. Two of those paths - the nested
binders in `Binder::bind_alone` and `dml.rs::bind_schema_expr` - also dropped
the connection's registered functions and collations on the way, so a schema
expression naming a registered function did not resolve at all; they inherit
them now.

**`CREATE INDEX` on an expression is refused when the index is created**, not on
the next write of the table. Such an index is filled by a `SELECT` the engine
builds out of the index's own expression, and a `SELECT` is a statement - so
that one query was the place a schema expression reached the machine with a
statement's permissions, and `CREATE INDEX i ON t (embed(body))` loaded the model
once per row before anything was refused.

**`PRAGMA trusted_schema` reports and sets the connection's own policy.** It
used to answer a constant 0 from the fixed-answer table while the connection's
policy said the opposite, which was harmless only for as long as nothing read
either one. The library's default is on, which is SQLite's; turning it off
refuses every registered function a schema names unless the registration said
`innocuous`.

**And `inillucent-shell` turns it off at startup**, which is what the reference's
shell does and why `.dbconfig` on the reference prints `trusted_schema off` on a
connection whose library default was on. A shell is a program that opens files it
did not write, which is the case the flag exists for. `.dbconfig trusted_schema`
reads and writes that setting now instead of printing a constant beside it.
`semantics.rs`'s `shell.dbconfig` case grades the whole listing against the
pinned SQLite and is what caught the difference.

**`PRAGMA defensive` refuses a write to a module's shadow table**, which is the
same defect in the same file: `Registry::authorize_shadow_write` was the whole of
that promise, it read a `Policy::defensive` nothing ever set, and nothing called
it. The shell turns defensive on for every connection it opens, so what
`.dbconfig defensive on` actually refused was `PRAGMA journal_mode = OFF` and
nothing else. Which names are shadow tables is derived from the roots each module
was connected with rather than from the spelling, so `docs_backup` is still an
ordinary table beside `docs_data`.

`Registry::authorize_extension` still has no caller and that is not the same
defect: nothing in this engine loads a shared library. `load_extension(path)`
refuses every path and so does the shell's `.load`, and those two refusals are
what `crates/inillucent-compat/tests/schema_function_policy.rs` checks, because
they are the guarantee a caller has. There is no `authorize_module` at all.

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

- A `CHECK`, a `DEFAULT`, a generated column or an index expression that names a
  function a schema may not name is refused when the statement that reads it is
  bound, not when the schema object is created. SQLite refuses the `CREATE`
  itself. `CREATE INDEX` is the exception and is refused at creation, because
  that is the one form this engine binds while it builds it.
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

## 0.1.3 — 2026-09-15, published 2026-09-19

**Published.** https://github.com/Black-Rainbow-Labs/Inillucent/releases/tag/v0.1.3
carries eleven assets and inillucent.com serves nine downloads, all naming 0.1.3.
This is the first inillucent release with macOS binaries: `inillucent-0.1.3.pkg`
is signed with a Developer ID and notarised by Apple, and
`inillucent-0.1.3-universal-apple-darwin.tar.gz` holds the same universal
binaries. It is also the first with signed `.deb` and `.rpm` packages, for x86-64
and aarch64, and the first whose `SHA256SUMS` carries a signature anyone can
check: `SHA256SUMS.minisig`, against `packaging/inillucent.pub`.

It was tagged on 2026-09-15 and left unpublished for four days. The GitHub
release stayed a **draft**, which is worse than nothing having happened: `gh
release view` finds a draft, so the release step uploaded every asset into it and
reported success while the release stayed invisible and untagged.
`Publish-GitHubRelease` publishes a draft it uploaded into now, and
`packaging/ship.ps1`'s preflight asks GitHub who it is rather than checking that
`gh` is installed — `gh` had never been logged in on the release machine.

This entry is written after the fact, because the release that cut the tag did
not write one and a hole between 0.1.2 and 0.1.4 is the kind of thing a reader
assumes is a mistake in their checkout.

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
