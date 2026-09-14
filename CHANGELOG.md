# Changelog

Every released version, what it was for, and what it is known not to do. The
dates are the dates the release was cut.

The version is the workspace's, which every package carries: the command line,
the shell, the MCP server, the migration tool, the C ABI library, and the Go,
npm, PyPI and Composer wrappers are all one number. `tools/doc-facts/check.mjs`
fails the build when any copy of it disagrees.

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
