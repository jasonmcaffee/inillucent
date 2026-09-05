# Dependency and provenance policy

inillucent is a first-party database engine. This file records what that means in
practice, what a production crate is allowed to depend on, and how a new
dependency is argued for. It is not advisory: `docs/invariants/layering.toml`
encodes the same rules and `cargo test -p inillucent-compat` fails on a violation.

## The ownership rule

An implementation agent must be able to build and test inillucent with **no SQLite,
Turso, libSQL, DuckDB, or other database engine installed**.

Production crates may not link to, invoke, translate, vendor, or generate code
from another database engine. The compatibility harness may launch a pinned
SQLite binary as a **separate child process** and compare serialized
observations. That binary is a test oracle, not a runtime component.

The subsystems below are first-party and cannot be delegated to an external
library: the SQL front end, semantic analysis, the catalog, planning, execution,
values, storage, transactions, SQL features, extensibility, the public
interfaces, and the assurance machinery itself.

## What a production crate may depend on

The allow-list lives in `docs/invariants/layering.toml`, one `[[external]]` row
per crate, naming the crates that may use it. The categories are:

| Category | Why it is infrastructure |
|---|---|
| error and data plumbing | carries values around; decides nothing about SQL or storage |
| synchronization | wrapped behind inillucent types; the locking protocol is ours |
| OS boundary | `libc` and `windows-sys`, used only inside `inillucent-vfs`, every call audited |
| hash and checksum | the algorithm and its on-disk use are specified by inillucent |
| async adaptation | the core owns polling and cancellation; an adapter only drives it |
| unicode helpers | tables and normalisation data; never SQL or collation semantics |
| numeric kernels | the existing retrieval engine's maths; no database behaviour |
| test-only | may not enter a production feature graph at all |

Two of the phase 1 crates take no third-party dependency at all. `inillucent-base`
has none, and `inillucent-vfs` has only the operating-system boundary. That is
deliberate: everything above them inherits their failure modes, so they should
have as few as possible.

### What is disallowed outright

SQL parsers, database engines, storage engines, B-tree or LSM libraries,
transaction managers, WAL implementations, query optimizers, and SQLite
bindings. `docs/invariants/layering.toml` lists the specific names, and the
check matches on substrings so a rename does not slip past.

### Adding a dependency

1. Add an `[[external]]` row naming the crate, its category, and every
   first-party crate that may use it.
2. Record here, in one or two sentences, why it is infrastructure rather than
   delegated database behaviour.
3. Run `cargo run -p inillucent-compat --bin inillucent-manifest -- layering`.

A crate that is not in the allow-list is refused even when it is harmless. The
policy is an allow-list rather than a deny-list because the interesting mistake
is not "someone added a bad crate" but "someone added a reasonable-looking crate
that quietly implements a piece of the engine".

## Deliberate additions made in task-1782

| Crate | Category | Reason |
|---|---|---|
| `libc` | OS boundary | `fcntl` byte-range locking and nothing else; `inillucent-vfs` only |
| `windows-sys` | OS boundary | `LockFileEx`, `GetFileInformationByHandle`, `BCryptGenRandom`; `inillucent-vfs` only |

Nothing else was added. SHA-256, SHA3-256, CRC-32, the WAL checksum, the varint
codec, the deterministic generator, the TOML subset reader and the JSON the
harness emits are all first-party, because each of them is part of a contract -
an on-disk format, a published checksum, an evidence artifact - that must not
change shape when a dependency is upgraded.

## The clean-reference workflow

External implementations may answer *what behaviour exists* and *what failure
did another project encounter*. They may not answer *copy this source*.

For a compatibility edge case: add a black-box oracle test first, record the
SQLite observation as structured data, then implement from the observed
contract. When public documentation and the oracle disagree, pin the behaviour
to the reference build and open a manifest decision rather than reading
implementation source to transplant an algorithm.

`docs/reference-register.toml` records every external project consulted, what
kind of reference it is, and whether it is a production dependency. Nothing in
it is.

## Unsafe code

`inillucent-base` forbids `unsafe` outright. `inillucent-vfs` permits it only in
`src/os/windows.rs` and `src/os/unix.rs`, where every block carries a written
safety argument and no raw pointer outlives the call it was made for. Every
other production crate is expected to forbid it; a crate that needs an exception
records the reason at the top of the file that uses it.

## Panics on untrusted input

`panic!`, unchecked indexing, `unwrap`, and `expect` are forbidden on the paths
that read SQL text, database pages, journal frames, or VFS results. The phase 1
crates enforce this with `deny(clippy::indexing_slicing)`,
`deny(clippy::unwrap_used)`, `deny(clippy::expect_used)` and
`deny(clippy::panic)`, relaxed only inside `#[cfg(test)]`.
