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
| OS boundary | `libc` and `windows-sys`, used only inside `inillucent-vfs` and, test-only, inside `inillucent-compat`'s `procstat.rs`; every call audited |
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
| `libc` | OS boundary | `fcntl` byte-range locking in `inillucent-vfs`; `getrusage` in `inillucent-compat`'s `procstat.rs`, so the gate can report what each arm's process cost (task-1838) |
| `windows-sys` | OS boundary | `LockFileEx`, `GetFileInformationByHandle`, `BCryptGenRandom` in `inillucent-vfs`; `GetProcessMemoryInfo` and `GetProcessTimes` in `inillucent-compat`'s `procstat.rs` (task-1838) |

Nothing else was added. SHA-256, SHA3-256, CRC-32, the WAL checksum, the varint
codec, the deterministic generator, the TOML subset reader and the JSON the
harness emits are all first-party, because each of them is part of a contract -
an on-disk format, a published checksum, an evidence artifact - that must not
change shape when a dependency is upgraded.

## The dependency task-1868 did not add

`inillucent migrate --kind postgres` and `--kind mysql` read a **running
server** over its own wire protocol. The obvious implementation is the
`postgres` crate - already on the allow-list above, as a benchmark baseline -
plus a `mysql` one, and it was rejected. This section records why, because the
next person to want a network client will reach for the same thing.

1. **It would put an async runtime in a shipped binary.** `postgres` 0.19 is a
   synchronous facade over `tokio-postgres`, so it brings Tokio; `mysql` brings
   its own tree. The crate that needs the client is linked by
   `inillucent-cli`, whose peak resident set is a number this project publishes
   and grades itself against.
2. **`mysql` would be a genuinely new dependency**, argued for on the grounds
   that it is "just a client". So is a SQL parser, from a certain angle. The
   allow-list exists because the interesting mistake is the reasonable-looking
   crate.
3. **Neither is needed.** The protocols are documented and stable, and the
   subset a *reader* needs is small: a startup packet, an authentication
   exchange, a query, and a text-format result set. `inillucent-remote` is that,
   in about two thousand lines, with **no third-party dependency at all**.

What it does own, which is the part worth checking, is four cryptographic
primitives the two logins are specified in terms of - MD5 (PostgreSQL `md5`),
SHA-1 (`mysql_native_password`), HMAC-SHA-256 and PBKDF2-HMAC-SHA-256
(SCRAM-SHA-256) - plus base64. They live in `inillucent-remote/src/auth.rs`
rather than in `inillucent-base` on purpose: they are **other people's wire
formats**, not contracts this engine publishes, and nothing on a page or in a
manifest is hashed with any of them. Each is checked against a published vector
in that file's own tests, because a hash that is subtly wrong does not produce a
wrong answer - it produces "password authentication failed", which reads as the
operator's mistake.

The limits that choice accepts are stated rather than hidden: no TLS
(`sslmode=require` is refused by name), and no `caching_sha2_password` **full**
authentication, whose RSA exchange is refused with the two ways around it.

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
