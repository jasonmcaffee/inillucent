# Security

## Reporting a vulnerability

**Use GitHub's private vulnerability reporting on this repository.** Open the *Security* tab, then
choose *Report a vulnerability*. That opens a private thread that only the maintainers can read.

If private reporting is not available to you, open a public issue that says only that you have
something to report and asks for a private channel. **Do not put the details in a public issue.**

**What to expect:**

| When | What you get |
|---|---|
| within **five working days** | a first reply that says whether the report is understood and can be reproduced |
| within **fifteen working days** | an assessment: the impact, whether a fix is coming, and when |
| the next release | the fix, and the published advisory |

If a report gets no reply within those times, it was missed by mistake. Say so on the thread.

You are credited under the name you ask for, or not credited if you prefer.

## What is in scope

inillucent parses SQL and opens files that may come from someone you do not trust. Most security
problems come from those two inputs.

- **A crafted `.rdb` file** that makes the engine read out of bounds, allocate without limit, loop
  forever, or return bytes from another file. Every code path that reads a page, a log frame or a
  network byte avoids `unwrap`, `expect`, `panic!` and slice indexing. All 29 crates deny all four.
  A panic caused by a file is a defect, and a report of one is in scope.
- **A crafted SQL statement** that does any of the above, or that gets past a limit the connection
  set.
- **Escaping `--root`.** The command line and the MCP server can be confined to one directory with
  `--root DIR`. A path that reaches a file outside `DIR` is a vulnerability.
  `crates/inillucent-compat/tests/e2e/confinement.rs` tests that no command can do this.
- **The migration transport.** `inillucent migrate` connects to PostgreSQL and MySQL over TLS. These
  are in scope: a downgrade the caller did not ask for, a certificate that is not checked, and a
  password that appears in a log, a manifest, a report or an MCP result.
- **The C ABI.** A lifetime or ownership mistake that correct use of the published header can reach.

## What is not in scope

- **A slow query you wrote yourself.** An embedded database runs in your process and does what you
  ask. A `CROSS JOIN` of three large tables is slow because the query asked for it. Use the
  connection's limits and budget to cap the work.
- **Anything that needs write access to the database file.** A caller who can write the file can
  put anything in it. No engine defends against that.
- **The dot commands that `inillucent-shell -safe` refuses**, such as `.shell`, `.system` and
  `.load`. Without `-safe`, `inillucent-shell` runs them, the same as `sqlite3` does.
- **A missing feature.** Exit code 3 means the engine has not built that feature. Exit code 1 means
  a real failure. The two codes are separate so a caller can tell them apart.

## What the repository already tests

This list shows a reporter what has already been checked.

- **Sixteen fuzz targets** under `fuzz/`. They cover the file format, the log, the SQL parser, the
  full text query parser, the record codec, JSON, and the PostgreSQL and MySQL protocols. Nothing
  runs them on a schedule. `fuzz/README.md` has the commands to run them by hand. Each target also
  has a stable test with a seeded random generator, listed in `fuzz/README.md`. The stable tests run
  in the ordinary suite on every checkout.
- **Crash campaigns** that cut the power at every file system call a run makes. Each campaign checks
  that the database reopens in one of the two states it is allowed to be in: before the commit or
  after it.
- **A confinement suite** that tries to make a confined process open a file outside its root,
  through every command and every pragma that takes a path.
- **`cargo deny check`** in the gate (`tools/validate.ps1` and `tools/validate.sh`). It checks
  security advisories, licences and the resolved dependency graph. `deny.toml` in the repository
  root lists what is allowed and why.
- **A dependency policy** that refuses another database engine, SQL parser, storage engine, B-tree,
  transaction manager, log or query optimizer. `cargo test -p inillucent-compat --test tooling policy::`
  enforces the dependency policy.

## Supported versions

Only the latest release gets security fixes. Fixes are not backported to older releases.
