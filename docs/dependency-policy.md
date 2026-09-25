# Dependency and provenance policy

inillucent is a first party database engine. This page says what a crate in this repository may
depend on, which third party crates are allowed, and how to argue for a new one. It also covers the
rules on `unsafe` code and on panics.

The rules are checked. `docs/invariants/layering.toml` states them as data, and
`cargo test -p inillucent-compat` fails when a crate breaks one.

## Terms used on this page

| Term | Meaning |
|---|---|
| crate | a Rust package. The workspace has 29 of them |
| production crate | a crate that ends up in a program or library inillucent ships |
| test only crate | a crate used only by tests and benchmarks, such as `inillucent-compat` |
| third party crate | a crate from crates.io, written outside this repository |
| operating system boundary | the calls a program makes into the operating system: file locks, memory maps, random bytes, TLS |
| FFI | a call from Rust into a C library, such as the Windows API or OpenSSL |
| TLS | the encryption layer under HTTPS and under an encrypted database connection |

Other terms are in [the glossary](glossary.md).

## The ownership rule

A developer must be able to build and test inillucent with **no SQLite, Turso, libSQL, DuckDB or
other database engine installed**.

A production crate may not link, call, translate, copy or generate code from another database
engine. The test harness may start a pinned SQLite program as a separate child process and compare
its answers with inillucent's. That SQLite program is a reference for tests. It is never part of a
shipped program.

These parts of the engine are first party and are never taken from a third party crate: the SQL
parser, semantic analysis, the catalog, the query planner, execution, values, storage,
transactions, SQL features, extensions, the public interfaces, and the test machinery that checks
them.

## What is never allowed

A production crate may not depend on a SQL parser, a database engine, a storage engine, a B-tree or
LSM library, a transaction manager, a write ahead log, a query optimizer or SQLite bindings.

`docs/invariants/layering.toml` lists the refused names in its `[[forbidden]]` rows:

| Pattern | Reason given in `layering.toml` |
|---|---|
| `sqlite`, `rusqlite` | SQLite is a test reference only |
| `libsql`, `turso`, `limbo` | SQLite forks and Turso's engine |
| `duckdb` | another database engine |
| `sqlparser` | inillucent writes its own SQL parser |
| `sqlx`, `diesel` | each brings a driver and a query layer |
| `rocksdb`, `redb`, `sled`, `lmdb`, `heed` | other storage engines |

The check matches substrings, so a renamed fork of one of these crates is still refused.

## The allowed list

Every third party crate a workspace crate uses has an `[[external]]` row in
`docs/invariants/layering.toml`. The row names the crate, its category, and every workspace crate
allowed to use it. A crate with no row is refused, even when it is harmless. The policy uses an
allowed list because the likely mistake is a reasonable looking crate that quietly does part of the
engine's job.

These are the rows in `layering.toml` today:

| Crate | Category | Allowed in |
|---|---|---|
| `libc` | operating system boundary | `inillucent-vfs`, `inillucent-remote`, `inillucent-cli`, `inillucent-compat` |
| `windows-sys` | operating system boundary | `inillucent-vfs`, `inillucent-remote`, `inillucent-cli`, `inillucent-compat` |
| `memmap2` | operating system boundary | `inillucent-core` |
| `anyhow`, `thiserror` | error handling | `inillucent-core`, `inillucent-bench` |
| `serde`, `serde_json` | data handling | `inillucent-core`, `inillucent-bench` |
| `rayon`, `rand`, `ort`, `tokenizers` | numeric kernel | `inillucent-core`, `inillucent-bench` |
| `bytemuck` | numeric kernel | `inillucent-core` |
| `rust-stemmers` | numeric kernel | `inillucent-core`, `inillucent-ext` |
| `clap` | command line parsing | `inillucent-bench` |
| `postgres`, `pgvector` | benchmark baseline | `inillucent-bench` |

`inillucent-core` is the retrieval engine: vector search, keyword search and embeddings. The numeric
kernels do its arithmetic and tokenizing and decide nothing about SQL or storage.
`inillucent-bench` is a benchmark program. It uses `postgres` and `pgvector` to measure the
PostgreSQL comparison, and no shipped program links `inillucent-bench`.

`rust-stemmers` is also allowed in `inillucent-ext` for the FTS5 `porter` tokenizer. The same Porter
stemmer is already in `inillucent-core`, and a second copy in `inillucent-ext` would repeat it.

The lowest crates take almost nothing. `inillucent-base` has no third party dependency.
`inillucent-vfs` has only the operating system boundary. Every other crate builds on these two, so
they have the fewest dependencies.

### The two operating system crates

`libc` and `windows-sys` are how Rust calls the operating system. Each call has a written safety
argument beside it.

| Crate that uses them | What for |
|---|---|
| `inillucent-vfs` | file locks (`fcntl` on Unix, `LockFileEx` on Windows), shared memory maps, file identity (`GetFileInformationByHandle`), random bytes (`BCryptGenRandom`), and the local time zone for the `utc` and `localtime` date modifiers |
| `inillucent-remote` | TLS for the PostgreSQL and MySQL migration clients and for downloads. Windows uses SChannel. Unix loads the system OpenSSL at run time |
| `inillucent-cli` | Ctrl+C handling in `src/interrupt.rs`: `SetConsoleCtrlHandler` on Windows and `signal` on Unix. The handler sets the cancel flag the executor already checks |
| `inillucent-compat` (test only) | the memory and processor time a benchmark process used (`getrusage`, `GetProcessMemoryInfo`, `GetProcessTimes`), and which processor cores a benchmark runs on |

### Code that is first party on purpose

These are all written in this repository: SHA-256, SHA3-256, CRC-32, the write ahead log checksum,
the varint codec, the deterministic random generator, the TOML reader and the JSON writer. Each one
is part of something inillucent publishes, such as a file format or a checksum. A dependency upgrade
must not be able to change any of them.

## Adding a dependency

1. Add an `[[external]]` row to `docs/invariants/layering.toml`. Name the crate, its category, and
   every workspace crate that may use it.
2. Add a row to this page that says in one or two sentences why the crate is infrastructure and does
   no database work.
3. Run the check:

   ```sh
   cargo run -p inillucent-compat --bin inillucent-manifest -- layering
   ```

Expect the answer to be no for most crates. The next sections are two worked examples of a
dependency that was refused and written here instead.

## Example: the PostgreSQL and MySQL clients

`inillucent migrate --kind postgres` and `--kind mysql` read a running server over its own network
protocol. The obvious way to build that is the `postgres` crate, which is already allowed for
benchmarks, plus a `mysql` crate. Both were refused:

1. **They bring an async runtime into a shipped program.** `postgres` 0.19 runs on Tokio, and the
   `mysql` crate brings its own set of dependencies. The client code is linked into `inillucent-cli`,
   and the peak memory of `inillucent-cli` is a number this project publishes and measures.
2. **A `mysql` crate would be a new dependency** with the argument that it is "just a client". The
   allowed list exists to stop reasonable looking crates that do engine work.
3. **Neither crate is needed.** The two protocols are documented and stable. A reader needs a small
   part of each: a startup message, a login, a query and a text result. `crates/inillucent-remote`
   is that part, with no third party crate apart from `libc` and `windows-sys` for TLS.

`inillucent-remote` does write four cryptographic functions that the two logins need: MD5 for
PostgreSQL `md5` logins, SHA-1 for `mysql_native_password`, and HMAC-SHA-256 with
PBKDF2-HMAC-SHA-256 for SCRAM-SHA-256. It also writes base64. They are in
`crates/inillucent-remote/src/auth.rs`. They are not in `inillucent-base` because they belong to
other projects' protocols, and inillucent hashes nothing it stores with them. Each function is tested
against a published test vector in `auth.rs`. A slightly wrong hash does not return a wrong answer.
It returns "password authentication failed", which looks like the operator's mistake.

One login is refused: MySQL's `caching_sha2_password` when the server asks for the full RSA exchange.
The error message names the two ways around it. [Migrating](migrating.md#mysql-caching_sha2_password)
has them.

### TLS without a TLS crate

The migration clients need TLS so that a password and the rows are encrypted on the network. Three
choices were considered:

| Choice | Result |
|---|---|
| `rustls` | refused. It is a large production dependency with its own cryptography, in a crate `inillucent-cli` links |
| TLS written in this repository | refused. A new certificate checker and record layer would be a larger security risk than the problem it solves |
| the operating system's TLS, through `libc` and `windows-sys` | chosen |

- **Windows** uses SChannel through SSPI. Windows checks the certificate chain and the host name with
  the same code every other program on the machine uses. `sslrootcert=<file>` makes the named
  certificate authority the only trusted root for that connection.
- **Unix** loads the system OpenSSL with `dlopen` when the program runs. The build does not link it,
  so a machine without the OpenSSL development package can still build inillucent.
  `SSL_set1_host` checks the host name. An OpenSSL older than 1.1.0 has no `SSL_set1_host`, and
  inillucent refuses to use it.

No cryptography for TLS is written in this repository. `inillucent-remote` uses
`deny(unsafe_code)` instead of `forbid(unsafe_code)` for the two files that make these calls,
`src/tls/windows.rs` and `src/tls/unix.rs`. Every `unsafe` block in them has a `SAFETY:` note, and
`crates/inillucent-compat/tests/tooling/policy.rs` checks that each note is there.

**A machine with no usable TLS gets an error that says so.** The client does not fall back to an
unencrypted connection.

## Example: downloads for `setup-embeddings`

`inillucent setup-embeddings` downloads ONNX Runtime from GitHub and the model weights from Hugging
Face. It checks both against pinned SHA-256 digests, then takes one shared library out of a zip file
or a gzipped tar file. None of that added a dependency.

An HTTP client crate was refused for the same reason as the `postgres` crate. Each one brings a TLS
library, and the popular ones bring an async runtime, into a program whose peak memory is published.
The command runs once per machine.

The parts were already in the repository:

- `inillucent-remote` has a socket with limits on how much it reads, and the operating system's TLS.
- `inillucent_base::deflate::inflate` decompresses zip and gzip data.

What was left to write is in `crates/inillucent-remote/src/http.rs` and `src/archive.rs`: a `GET`
request, two ways of reading a response body, redirects, a `Range` header, the zip central directory
and the tar header.

These are not supported, and each is refused by name when it appears:

- HTTP proxies, cookies, authentication and compression. The request sends
  `Accept-Encoding: identity` so the bytes received are the bytes that are checked.
- zip64, encrypted zip files, and compression methods other than stored and deflated.
- tar extensions other than the GNU long name record.

Two rules protect the files on disk:

- **A file that fails its digest is never left in place.** The download goes to `<destination>.part`
  and is renamed only after the digest matches. An interrupted run leaves a `.part` file that the
  next run continues.
- **An archive member whose name leaves the destination folder stops the extraction.** A name such
  as `../escape` is refused, and the name is never cleaned into a different file name. A test in
  `archive.rs` checks this.

## The two retired storage crates

The storage engine was rewritten. `inillucent-pool`, `inillucent-tree`, `inillucent-wal`,
`inillucent-txn` and `inillucent-exec` replaced `inillucent-storage` and `inillucent-transaction`.

`inillucent-storage` and `inillucent-transaction` are still in the workspace for one reason.
`inillucent-sqlite-reader` reads the SQLite file format through them, and `inillucent migrate` uses
`inillucent-sqlite-reader` to read a SQLite file.

`policy.rs::no_new_crate_reaches_into_the_retired_engine` lists by name the crates that may still
depend on the two retired crates. A crate that is not on that list fails the test when it adds such
a dependency. Removing a name from the list is the goal. Adding a name needs an argument in review.

| Crate | Why it still depends on a retired crate | What removing the dependency needs |
|---|---|---|
| `inillucent-catalog` | an old schema reader, which the new engine's `paged` module already replaces | delete the old reader once nothing calls it |
| `inillucent-sqlite-reader` | it reads SQLite's file format through the old pager | a second B-tree reader |
| `inillucent-compat` | test only. It reads SQLite files to compare the two engines | nothing. A test only crate is never in a shipped program |

## Using other projects as a reference

Another project's code or documentation may answer two questions: what behavior exists, and what
failures that project ran into. Nobody copies another project's source into inillucent.

For a SQLite compatibility case:

1. Write a test that runs the pinned SQLite program as a child process and records its answer.
2. Implement the behavior in inillucent from that recorded answer.
3. When SQLite's documentation and the pinned SQLite program disagree, follow the program. Record
   the decision in a manifest. Do not read SQLite's source to copy an algorithm.

`docs/reference-register.toml` lists every outside project that was consulted and what kind of
reference it is. Every entry has `production_dependency = false`, and
`policy.rs::no_reference_is_a_production_dependency` checks that.

## Unsafe code

21 of the 29 crates forbid `unsafe` with `#![forbid(unsafe_code)]`. The others use `unsafe` only in
the files named in `UNSAFE_ALLOWED` in `crates/inillucent-compat/tests/tooling/policy.rs`. Examples are the
allocator in `inillucent-alloc`, the operating system calls in `inillucent-vfs`, the TLS files in
`inillucent-remote`, the Ctrl+C handler in `inillucent-cli`, and the AVX2 dot product in
`crates/inillucent-core/src/distance.rs`.

Every `unsafe` block in those files has a `SAFETY:` comment, and every `unsafe` function has a
`# Safety` section. `policy.rs` fails when one is missing, or when `unsafe` appears in a file that is
not on the list.

## Panics on untrusted input

A panic stops the program. So the code that reads SQL text, database pages, journal frames, network
bytes or file system results may not use `panic!`, `unwrap`, `expect` or unchecked slice indexing.
The crates enforce this with four lints:

```rust
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
```

The four lints are relaxed only inside `#[cfg(test)]`.

## Where to go next

- [Repository layout](repository.md): the crates and their layers
- [Migrating](migrating.md): the PostgreSQL and MySQL clients from a user's side
- [Embeddings](embeddings.md): what `setup-embeddings` downloads
