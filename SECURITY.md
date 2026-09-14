# Security

## Reporting a vulnerability

**Use GitHub's private vulnerability reporting** on this repository: the
*Security* tab, then *Report a vulnerability*. That opens a private thread only
the maintainers can read, which is what you want for anything that should not be
in a public issue until there is a fix.

If that is unavailable to you, open a public issue saying only that you have
something to report and asking for a private channel. **Do not put the details
in a public issue.**

**What to expect.** A first reply within **five working days**, saying whether
the report is understood and reproducible. After that, an assessment within
**fifteen working days** — what the impact is, whether a fix is coming, and when.
If a report goes quiet past those windows, it is an oversight rather than a
decision; say so on the thread.

A fix ships in the next release, and the advisory is published when it does. You
are credited by whatever name you ask for, or not at all if you prefer.

## What is in scope

This program parses untrusted SQL and opens untrusted files, which is most of
the attack surface:

- **A crafted `.rdb` file** that makes the engine read out of bounds, allocate
  without bound, loop forever, or return another file's bytes. Every path that
  reads a page, a log frame or a network byte is written without `unwrap`,
  `expect`, `panic!` or slice indexing, and 28 of the 29 crates deny all four —
  so a panic reached from a file is a defect, not a hardening request.
- **A crafted SQL statement** that does the same, or that escapes a limit the
  connection set.
- **Escaping `--root`.** The command line and the MCP server can be confined to
  a directory; a path that reaches outside it is a vulnerability.
  `crates/inillucent-compat/tests/confinement.rs` is what asserts they cannot.
- **The migration tool's transport.** `inillucent migrate` speaks PostgreSQL and
  MySQL over TLS. A downgrade the caller did not ask for, a certificate that is
  not checked, or a password that reaches a log, a manifest, a report or an MCP
  result is in scope.
- **The C ABI.** A lifetime or ownership mistake reachable from correct use of
  the published header.

## What is not

- **A denial of service from a query you wrote yourself.** An embedded database
  runs in your process and does what you ask; a `CROSS JOIN` of three large
  tables is slow because you asked for it. Use the connection's limits and the
  budget.
- **Anything requiring write access to the database file.** A caller who can
  write the file can write anything into it, and no engine defends against that.
- **`PRAGMA`s and commands that are documented as unsafe**, which the shell
  refuses unless `.unsafe on` was typed.
- **A missing feature.** Exit code 3 means "this engine has not built that", and
  it is a different code from 1 on purpose.

## What the repository already does about this

Named here so a reporter knows what has been looked at rather than having to
guess:

- **Fuzz targets** under `fuzz/`, run on a schedule by
  `.github/workflows/fuzz.yml`, over the file format, the SQL parser and the
  record codec.
- **Crash campaigns** that cut the power at every call a run makes to the file
  system, and assert the database comes back as one of the two states it is
  allowed to be in.
- **A confinement suite** that tries to talk a confined server into opening a
  file outside its root, through every command and every pragma that takes a
  path.
- **`cargo deny check`** in the gate, over advisories, licences and the resolved
  dependency graph, with `deny.toml` at the root saying what is allowed and why.
- **A dependency policy** that refuses another database engine, SQL parser,
  storage engine, B-tree, transaction manager, log or query optimiser outright,
  enforced by a test rather than by review.

## Supported versions

The latest release only. This is pre-1.0 software and there is no back-porting;
a fix ships forward.
