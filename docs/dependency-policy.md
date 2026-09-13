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

## The deliberate additions

| Crate | Category | Reason |
|---|---|---|
| `libc` | OS boundary | `fcntl` byte-range locking in `inillucent-vfs`; `getrusage` in `inillucent-compat`'s `procstat.rs`, so the gate can report what each arm's process cost |
| `windows-sys` | OS boundary | `LockFileEx`, `GetFileInformationByHandle`, `BCryptGenRandom` in `inillucent-vfs`; `GetProcessMemoryInfo` and `GetProcessTimes` in `inillucent-compat`'s `procstat.rs`; SChannel and the certificate chain engine in `inillucent-remote`'s `tls` |

Nothing else was added. SHA-256, SHA3-256, CRC-32, the WAL checksum, the varint
codec, the deterministic generator, the TOML subset reader and the JSON the
harness emits are all first-party, because each of them is part of a contract -
an on-disk format, a published checksum, an evidence artifact - that must not
change shape when a dependency is upgraded.

## The dependency that was deliberately not added

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

The limit that choice still accepts is `caching_sha2_password` **full**
authentication, whose RSA exchange is refused with the two ways around it.

### TLS, and why it is not a crate either (task-1894)

The first version of this had **no TLS**: `sslmode=require` was refused by name
and a migration to a server across a network sent its password and then every
row in the clear. The review that found it is `task-1892`, and the fix had three
candidates.

`rustls` is the obvious crate and it was rejected on the argument two paragraphs
up: it is a large production dependency with a cryptographic backend of its own,
in a crate `inillucent-cli` links. Writing TLS here is not a real option, and
saying so is the point - a first-party X.509 chain builder and record layer
would be a far larger security surface than the plaintext migration it replaced.

What is left is the row already on this list. Every platform this ships on has
an audited TLS implementation and a trust store somebody else keeps current, and
reaching one is an FFI call:

- **Windows** uses SChannel through SSPI. The chain and the host name are
  checked by Windows, with the same code every other program on the machine
  uses. `sslrootcert=` switches to a chain engine whose `hExclusiveRoot` is the
  named authority, which is *stricter* than the machine store rather than
  weaker - nothing else is trusted for that connection.
- **Unix** uses the system OpenSSL, loaded with `dlopen` at run time rather than
  linked. Linking it would make a machine without the development package unable
  to build the engine; loading it moves "is TLS available" to run time, where the
  answer actually lives. `SSL_set1_host` does the host name check, and a library
  too old to have it is refused rather than used without it.

No cryptography is implemented in this workspace by either. `inillucent-remote`
carries `deny(unsafe_code)` rather than `forbid` for exactly the two files that
make those calls, and every block in them has a SAFETY note that
`crates/inillucent-compat/tests/policy.rs` checks.

**A machine with no usable TLS refuses and says so.** It does not fall back:
falling back is the defect this replaced, and a fallback nobody sees is worse
than the refusal.

### The HTTP client, and the two archive formats (task-1900)

`inillucent setup-embeddings` downloads ONNX Runtime from GitHub and the weights from Hugging Face,
verifies both against pinned SHA-256 digests, and takes one shared library out of a zip or a gzipped
tar. None of that added a dependency, and the reasoning is the same as the paragraph above.

**An HTTP client crate was the obvious answer and is the wrong one here.** Every one of them brings a
TLS backend, and the popular ones bring an async runtime, into a binary whose peak resident set is a
published number — for a command that runs once per machine. The argument the `postgres` client lost
is the argument this one loses.

**And the pieces were already here.** `inillucent-remote` has the socket with bounded reads and the
platform's own verified TLS, because the migration clients needed both.
`inillucent_base::deflate::inflate` is the whole of zip's and gzip's decompression and is already
checked against the format. What was left to write is a `GET`, two body framings, redirects, a
`Range` header, and two container formats — a zip central directory and a tar header — and none of
those is a place where a third-party crate carries knowledge this repository does not have.

What is deliberately **not** implemented is as much of the argument as what is. No proxies, no
cookies, no authentication, no compression negotiation — `Accept-Encoding: identity` is sent
precisely so the bytes on the wire are the bytes being digested. No zip64, no encryption, no
compression method beyond stored and deflated, no tar extension past the GNU long-name record. Each
of those is refused by name if it appears, so an archive this cannot read is a message rather than a
wrong file.

Two properties matter here, because they are what a downloader gets wrong:

- **A file that fails its digest is not left on disk.** The download is written to a `.part` beside
  the destination and renamed only once the digest matches, so an interrupted run leaves something a
  later run resumes and never leaves a complete-looking file that is not.
- **An archive member whose name escapes the destination stops the extraction.** Not sanitised into
  something harmless-looking: stripping the `..` out of `../../etc/passwd` produces a file nobody
  asked for under a name that looks deliberate. A test builds an archive with `../escape` in it.


## The edges into the retired engine, and the ratchet on them

The rearchitecture (task-1816) replaced `inillucent-storage`, `inillucent-transaction`
and `inillucent-vm` with `inillucent-pool`, `inillucent-tree`, `inillucent-wal`,
`inillucent-txn` and `inillucent-exec`. `inillucent-vm` is gone from the workspace
now, along with `inillucent-session`, `inillucent-legacy` and `inillucent-capi` -
the engine that reached SQLite file-format parity, and the connection, facade and
C ABI built over it. Nothing outside `inillucent-compat`'s own test and profiling
binaries named any of the four, so their deletion changed no shipped binary's
dependency graph. `inillucent-storage` and `inillucent-transaction` stay: they are
what `inillucent-sqlite-reader` reads a SQLite file through, and migrating away
from SQLite is what that reader is for. Prose does not stop a new edge to *those*
two: the way a crate acquires one is that somebody adds a line to a manifest
because the type they wanted lives there, and nothing says no.

So the crates that may still name one of the two are **listed by name in a test**
(`policy.rs::no_new_crate_reaches_into_the_retired_engine`). A crate that is not
on the list fails the moment it grows the edge, and the list only ever gets
shorter: removing a name is the work, adding one is a decision somebody has to
argue for in a review.

| crate | why it is still there | what removing it needs |
|---|---|---|
| `inillucent-catalog` | its old-engine schema reader, which the new engine's `paged` module already replaces | deleting the arm, once nothing calls it |
| `inillucent-sqlite-reader` | it reads **SQLite's** file format and uses the old pager *as* the format reader | a second b-tree reader, not a dependency edit |
| `inillucent-compat` | test-only; reading a SQLite file through `inillucent-storage`'s pager to migrate away from it is what it is for | nothing — a test-only crate cannot put an edge in a shipped binary |

**task-1894 removed `inillucent-ext`** from that list, which was the only entry
the *new* engine links — and therefore the only one that put two storage models
in a shipped binary rather than merely in the workspace. Its
`inillucent-transaction` edge was never used by a line of code. Its
`inillucent-storage` edge was two things: a pager arm inside every method of
`ShadowTables`, and an `impl Host for Pager`. Both moved down into what was then
`inillucent-vm`, behind `inillucent_sql::vtab::ShadowStore` — the trait the new
engine already implemented — so both engines reached a module's shadow rows the
same way, and now only the two storage crates the migration path still needs are
left to name.

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
