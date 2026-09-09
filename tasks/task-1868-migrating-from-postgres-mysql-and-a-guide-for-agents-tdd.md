# task-1868 — Migrating from PostgreSQL and MySQL, and a front door for agents

## Introduction

`inillucent-migrate` can build an `.rdb` from two sources: a legacy retrieval index directory, and a
SQLite **file**. Both are files, and both are read by a first-party reader that already lives in this
workspace. The two databases people most often want to leave are neither of those: they are
**PostgreSQL** and **MySQL**, and they are *servers* — there is no file to open, so every byte has to
come over a socket, and getting to the first byte means speaking a wire protocol and authenticating.

This ticket adds that: a `migrate --kind postgres` and `--kind mysql` that connect to a running
server, read every ordinary table, build a verified `.rdb`, and publish it by rename — with the same
invariants the SQLite path already holds (**the source is never written**, the destination is
**never overwritten**, and nothing is published that has not been verified by count *and* digest).

It also adds the second half of the ticket: a front door for AI agents. The repository is large,
opinionated and full of rules that are enforced by tests, and an agent that has to discover them by
failing builds is an expensive agent. `AGENTS.md` plus an `agent-skills/` directory turns the things
agents repeatedly need — query a database, migrate one, embed the driver, run the right tests, add a
dependency without failing the layering check — into short, task-shaped instructions.

## Goals and Non-Goals

### Goals

| # | Goal | How it is measured |
|---|---|---|
| G1 | Migrate a live PostgreSQL database into an `.rdb` | Every ordinary table in every non-system schema, verified by row count and ordered-independent digest, published only when every check passes |
| G2 | Migrate a live MySQL/MariaDB database into an `.rdb` | Same |
| G3 | **No new third-party dependency** | `docs/invariants/layering.toml` gains a crate row and **no** `[[external]]` row; `cargo tree` for the new crate shows only first-party edges |
| G4 | The new kinds appear in **both** the CLI and MCP surfaces | They are rows in the one command table; `command_parity.rs` stays green |
| G5 | The wire clients are testable **with no server installed** | A recorded-transcript fake server in-process; the suites pass on a fresh clone |
| G6 | The migrations are proven against **real** servers | A live PostgreSQL 17 and a live MySQL, both migrated, both verified, evidence in the ticket |
| G7 | An agent can be useful in this repository in one read | `AGENTS.md` + `agent-skills/`, linked from the README |
| G8 | Every new test target is registered | `tests/selection.toml` rows; `crates/inillucent-compat/tests/selection.rs` green |

### Non-Goals

- **Migrating back out.** This is an import. There is no `.rdb` → PostgreSQL direction.
- **Schema fidelity beyond what the destination has.** PostgreSQL types this engine has no
  equivalent for (`numeric` of arbitrary precision, `interval`, arrays, ranges, composite types,
  enums) are carried as the server's own **text rendering**, losslessly as text, in a `TEXT` column.
  A migration that silently rounded a `numeric(38,10)` into an IEEE double would be the failure mode
  this project cares most about — a wrong answer nobody sees.
- **Constraints, triggers, sequences, views, functions, extensions.** Tables and rows are carried.
  Primary keys are carried where they map onto this dialect. Everything else is **reported, not
  invented**: one check per object naming what was not carried, so the report says so rather than
  leaving the reader to notice.
- **TLS.** A migration is a one-off run against a database you already have credentials for, usually
  on a socket you control. `sslmode=require` is refused with a message naming the limitation rather
  than silently downgrading — see *Risks*.
- **Incremental / resumable remote migration.** The file sources resume from a manifest because a
  file does not change under you. A server does; a half-migrated table resumed an hour later would
  be half of one snapshot and half of another. A remote migration is one pass, inside one
  **repeatable-read snapshot** on the source, and a failure means run it again.

## Problem statement

Today `inillucent migrate` takes exactly one thing: a path to a SQLite file. The README positions
this engine as doing "the job of PostgreSQL + pgvector + an embedding server, in process", and the
scorecard grades it against pgvector — but there is no supported way to actually *move* a PostgreSQL
database into it. The only paths are hand-written: dump, transform, re-insert, hope. That is exactly
the shape of migration this repository already decided is unacceptable for SQLite, and the reasons
are in `crates/inillucent-migrate/src/sqlite.rs`: a migration that moves the right number of rows and
the wrong bytes passes a count check, and a half-written destination must never sit where an
application would open it.

The second problem is discoverability. The repository has a dependency allow-list enforced by a test,
a layering contract enforced by a test, a test-selection map enforced by a test, a command table that
two surfaces are generated from and enforced by a test, and a testing standard with its own runner.
None of that is discoverable from a `git clone`; all of it fails a build when an agent guesses. The
cost is paid once per agent, repeatedly.

## Architectural Overview

```mermaid
flowchart TB
  subgraph surfaces["Command surfaces - one table, two front ends"]
    CLI["inillucent migrate"]
    MCP["inillucent-mcp tool: migrate"]
    BIN["inillucent-migrate binary"]
  end

  subgraph remote["crate inillucent-remote - NEW, zero third-party deps"]
    URL["url.rs - connection URL"]
    AUTH["auth.rs - MD5, SHA-1, HMAC-SHA-256, PBKDF2, base64"]
    PG["postgres.rs - v3 frontend/backend protocol"]
    MY["mysql.rs - MySQL client protocol"]
    SRC["source.rs - RemoteSource trait, type mapping"]
    MIG["migrate.rs - inventory, stage, copy, verify, publish"]
  end

  subgraph engine["existing"]
    ENG["inillucent-engine - Database, Connection, Statement"]
    TREE["inillucent-tree - OwnedDatum"]
    BASE["inillucent-base - Sha256, DbResult"]
  end

  CLI --> MIG
  MCP --> MIG
  BIN --> MIG
  MIG --> SRC
  SRC --> PG
  SRC --> MY
  PG --> AUTH
  MY --> AUTH
  PG --> URL
  MY --> URL
  MIG --> ENG
  SRC --> TREE
  AUTH --> BASE

  PG -. TCP .-> PGS[("PostgreSQL server")]
  MY -. TCP .-> MYS[("MySQL / MariaDB server")]
  MIG ==> RDB[("destination.rdb")]
```

### Why a new crate rather than more of `inillucent-migrate`

`inillucent-migrate` depends on `inillucent-core` and `inillucent-search` — the whole retrieval
engine — because its *other* job is importing a legacy retrieval index. That is why
`inillucent migrate --kind index` today answers "runs in inillucent-migrate, which links the retrieval
engine" instead of doing the work. Putting the remote sources there would make the same thing true of
PostgreSQL, and G4 would be unreachable.

`inillucent-remote` therefore depends on **`inillucent-base`, `inillucent-tree` and
`inillucent-engine` only**, which is exactly what `inillucent-cli` already links. The CLI, the MCP
server and the standalone binary all call the same function.

**This is only possible because the protocol clients are first-party.** `postgres 0.19` is already in
the workspace as a benchmark baseline, and reaching for it here would have been the short road — but
it brings a Tokio runtime into a binary whose peak resident set is a published number
(`inillucent-shellrss`), and a `migrate` verb that linked another database's client library into the
shipped `inillucent` would contradict the sentence the README opens with. The protocols themselves
are documented, stable and small at the subset a reader needs.

## Detailed technical sections

### 1. `inillucent-remote::url` — one connection URL, two schemes

```
postgres://user:password@host:5432/dbname?sslmode=disable&connect_timeout=10
mysql://user:password@host:3306/dbname
```

Percent-decoding on every component, an IPv6 host in brackets, a default port per scheme, and a
`Display` that **redacts the password** — the same redaction `inillucent-bench` already does, because
a connection URL ends up in a report and a manifest. Missing user defaults to the OS user on
PostgreSQL and `root` on MySQL, matching each client's own convention.

### 2. `inillucent-remote::auth` — the primitives, first-party

| Primitive | Needed by | Checked against |
|---|---|---|
| MD5 | PostgreSQL `md5` authentication | RFC 1321 test vectors |
| SHA-1 | MySQL `mysql_native_password` | RFC 3174 / FIPS 180 vectors |
| HMAC-SHA-256 | PostgreSQL SCRAM-SHA-256 | RFC 4231 vectors |
| PBKDF2-HMAC-SHA-256 | PostgreSQL SCRAM-SHA-256 | RFC 7914 §11 / published vectors |
| Base64 | SCRAM message encoding | RFC 4648 vectors |
| SHA-256 | SCRAM, MySQL `caching_sha2_password` | already first-party in `inillucent-base` |

These live in the remote crate rather than in `inillucent-base` on purpose: MD5 and SHA-1 are *other
people's* wire formats, not contracts this engine publishes, and `inillucent-base` is the crate
everything inherits its failure modes from.

### 3. `inillucent-remote::postgres` — the v3 protocol, at the subset a reader needs

```mermaid
sequenceDiagram
  participant C as inillucent-remote
  participant S as PostgreSQL

  C->>S: StartupMessage {user, database, client_encoding=UTF8}
  alt trust
    S-->>C: AuthenticationOk
  else md5
    S-->>C: AuthenticationMD5Password(salt)
    C->>S: PasswordMessage("md5" + md5(md5(pass+user)+salt))
  else scram-sha-256
    S-->>C: AuthenticationSASL(SCRAM-SHA-256)
    C->>S: SASLInitialResponse(client-first-message)
    S-->>C: AuthenticationSASLContinue(server-first)
    C->>S: SASLResponse(client-final with proof)
    S-->>C: AuthenticationSASLFinal(server-signature verified by C)
    S-->>C: AuthenticationOk
  end
  S-->>C: ParameterStatus*, BackendKeyData, ReadyForQuery
  C->>S: Query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
  C->>S: Query(catalog query - tables and columns)
  S-->>C: RowDescription, DataRow*, CommandComplete, ReadyForQuery
  loop per table
    C->>S: Query("DECLARE c NO SCROLL CURSOR FOR SELECT ... ORDER BY ...")
    loop until fewer than N rows
      C->>S: Query("FETCH 10000 FROM c")
      S-->>C: RowDescription, DataRow*, CommandComplete
    end
    C->>S: Query("CLOSE c")
  end
  C->>S: Query("COMMIT"), Terminate
```

- **Simple query, text format, everywhere.** The extended protocol buys binary results and parameter
  binding; a reader that streams whole tables needs neither, and text is the format whose decoding is
  specified by the type's own output function rather than by a binary layout that varies by version.
- **One snapshot.** The whole read happens inside `REPEATABLE READ READ ONLY`, so every table is read
  as of one instant. Reading table B after table A committed is how a migration produces a database
  that never existed.
- **A cursor, not a whole result set.** `libpq` buffers an entire result; a `FETCH`-driven cursor
  bounds memory to one batch regardless of table size.
- **The catalog query** reads `information_schema.tables` / `.columns` restricted to
  `BASE TABLE`, excluding `pg_catalog` and `information_schema`. A schema other than `public` is
  carried into the destination table's name as `schema__table` (with a check saying so), because this
  dialect has one namespace per database.

**Errors are the protocol's own.** An `ErrorResponse` is decoded into its `S`/`C`/`M`/`D` fields and
surfaced as `corrupt("postgres 28P01: password authentication failed for user \"x\"")` — a message
that names the SQLSTATE, so a caller can act on it without matching English.

### 4. `inillucent-remote::mysql` — the client protocol

```mermaid
sequenceDiagram
  participant C as inillucent-remote
  participant S as MySQL / MariaDB

  S-->>C: HandshakeV10 {version, capabilities, auth plugin, 20-byte scramble}
  C->>S: HandshakeResponse41 {capabilities, charset utf8mb4, auth response}
  alt mysql_native_password
    note over C: SHA1(pass) XOR SHA1(scramble + SHA1(SHA1(pass)))
  else caching_sha2_password - fast path
    note over C: SHA256(pass) XOR SHA256(SHA256(SHA256(pass)) + scramble)
    S-->>C: AuthMoreData(0x03 fast auth succeeded)
  end
  S-->>C: OK_Packet
  C->>S: COM_QUERY "SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ"
  C->>S: COM_QUERY "START TRANSACTION WITH CONSISTENT SNAPSHOT"
  C->>S: COM_QUERY catalog query
  loop per table
    C->>S: COM_QUERY "SELECT * FROM `t`"
    S-->>C: column count, ColumnDefinition41*, ResultsetRow*, EOF/OK
  end
  C->>S: COM_QUERY "COMMIT", COM_QUIT
```

- **`caching_sha2_password` full auth is refused, not faked.** The fast path (the server already has
  the password cached, or the account is `mysql_native_password`) works over a plain socket. The
  *full* path requires either TLS or an RSA public-key exchange, and this client has neither. It
  produces a named refusal telling the operator to connect once with another client to prime the
  cache, or to use a `mysql_native_password` account for the migration. A silent failure here would
  look like "the server rejected my password", which is a wrong diagnosis.
- **Row streaming.** The text resultset arrives packet by packet; rows are consumed as they arrive
  rather than collected, so a 50-million-row table costs one row of memory plus the destination's
  batch.
- **The catalog query** reads `information_schema.tables` where `table_schema = <db>` and
  `table_type = 'BASE TABLE'`.
- MariaDB is the same protocol; its handshake is detected and reported in the manifest by server
  version string.

### 5. `inillucent-remote::source` — one shape, and the type map

```rust
/// One column of a source table, as the server describes it.
pub struct SourceColumn { pub name: String, pub declared: String, pub kind: Kind, pub nullable: bool }

/// How a source value is carried into this engine.
pub enum Kind { Integer, Real, Text, Blob, Boolean, Decimal }

/// One table of a source database.
pub struct SourceTable { pub schema: String, pub name: String, pub target: String,
                         pub columns: Vec<SourceColumn>, pub primary_key: Vec<String> }

/// A database somebody else is running.
pub trait RemoteSource {
    fn describe(&mut self) -> DbResult<Vec<SourceTable>>;
    fn count(&mut self, table: &SourceTable) -> DbResult<u64>;
    fn scan(&mut self, table: &SourceTable, sink: &mut dyn FnMut(&[OwnedDatum]) -> DbResult<()>) -> DbResult<u64>;
    fn describe_server(&self) -> String;
    fn not_carried(&mut self) -> DbResult<Vec<(String, String)>>;
}
```

`scan` **pushes** rows into a sink rather than returning a `Vec`, which is what makes "one row of
memory" true; the migration's sink binds and steps a prepared `INSERT` and folds the row into the
digest at the same time, so the source is read exactly once.

| source type | carried as | declared as | why |
|---|---|---|---|
| `smallint`, `integer`, `bigint`, `serial`, MySQL `TINYINT`…`BIGINT` (signed) | `Integer` | `INTEGER` | exact |
| `BIGINT UNSIGNED` above `i64::MAX` | `Text` | `TEXT` | there is no wider integer here, and rounding it into a double is a wrong answer |
| `real`, `double precision`, `FLOAT`, `DOUBLE` | `Real` | `REAL` | IEEE both sides |
| `numeric`/`decimal` | `Text` | `TEXT` | arbitrary precision; the server's own digits, unrounded |
| `boolean`, `TINYINT(1)` | `Integer` 0/1 | `INTEGER` | this dialect's own convention |
| `bytea`, `BLOB`, `BINARY`, `VARBINARY` | `Blob` | `BLOB` | `\x` hex decoded; MySQL binary bytes as they arrive |
| `text`, `varchar`, `char`, `uuid`, `json`, `jsonb`, dates, times, `interval`, arrays, ranges, enums, everything else | `Text` | `TEXT` | the server's own text rendering, byte for byte |
| SQL `NULL` | `Null` | — | the protocol's null marker, distinct from the string `"NULL"` |

The last row of that table is the one worth arguing with, so: the alternative is a translation table
per exotic type, and each entry is a place a migration can be wrong in a way that reads as data. The
text rendering is what the source's own `psql` prints, is exact, and is the one representation that
cannot lose information it had.

### 6. `inillucent-remote::migrate` — the procedure

The same eight steps `sqlite.rs` runs, minus the ones a server makes meaningless:

1. **Refuse a destination that exists.** A migration publishes by renaming and never overwrites.
2. **Connect, snapshot, describe.** One repeatable-read transaction for the whole read.
3. **Stage** into `.<name>.staging` beside the destination, so the publish is a same-filesystem
   rename and therefore atomic.
4. **Create the schema** — one `CREATE TABLE` per source table, columns in ordinal order, primary key
   carried when every one of its columns maps.
5. **Copy in bounded transactions.** 10,000 rows per commit through one prepared statement per table,
   digesting each row as it is bound.
6. **Checkpoint, close, and reopen from the file.** What is verified is what a fresh process sees,
   not what the writing pool saw.
7. **Verify.** Per table: the destination's row count against the source's `SELECT count(*)` — a
   *different query path* on the source than the scan — and the destination's digest against the
   digest folded during the copy. Plus `Database::check()`, which walks every tree in key order.
8. **Publish by rename**, log segments first, only if every check passed. A failure leaves the
   staging file and a written report, because the thing a person needs after a failed migration is
   the evidence.

**What the verification does and does not prove**, stated the way `sqlite.rs` states its own: the two
digests are computed from the same reader (the wire client) and the destination's own scan, so a
disagreement means the copy is wrong and agreement means what was read reached the destination
unchanged. It does *not* prove the wire decoding was right. That oracle is a third engine — `psql`
and `mysql` themselves — and it lives in the acceptance test rather than in the tool, because the
tool must not require a PostgreSQL installation in order to migrate away from one.

### 7. The command table

`crates/inillucent-cli/src/command/registry.rs` — the one table both surfaces are generated from.
`MIGRATE_PARAMS` gains nothing structurally; `kind` gains two values and `source` gains a sentence:

| param | change |
|---|---|
| `source` | "The SQLite database file to read, or a `postgres://` / `mysql://` connection URL." |
| `kind` | "'sqlite' (default), 'postgres', 'mysql', or 'index'." |
| `batch` | **new, optional** — rows per commit while copying; default 10,000 |

Because both surfaces read this table, `command_parity.rs` stays green without being touched, and the
MCP tool's JSON schema picks up the new values from the same strings.

**The password is not a parameter.** It comes in the URL or in `PGPASSWORD` / `MYSQL_PWD`, and the
outcome's `source` field is the **redacted** URL, because an MCP tool's arguments and result are
written into an agent transcript.

## Data flows and security

```mermaid
flowchart LR
  A["migrate --kind postgres postgres://u:p@h/db --destination out.rdb"] --> B{destination exists?}
  B -- yes --> X1["InvalidState: never overwrites"]
  B -- no --> C[connect + authenticate]
  C -- auth fails --> X2["named SQLSTATE / MySQL error"]
  C --> D[REPEATABLE READ snapshot]
  D --> E[describe: tables, columns, keys]
  E --> F["stage .out.rdb.staging"]
  F --> G[CREATE TABLE per source table]
  G --> H[copy, 10k rows per commit, digest while binding]
  H -- error --> X3["staging + report left in place"]
  H --> I[checkpoint, close]
  I --> J[reopen from file, check trees]
  J --> K[verify count + digest per table]
  K -- any check fails --> X4["report written, nothing published"]
  K -- all pass --> L[rename log segments, then the file]
  L --> M["out.rdb"]
```

**Risks and what is done about them**

| Risk | Mitigation |
|---|---|
| Credentials leaking into a transcript, a report or a manifest | one `Display`, redacting; the raw URL is never formatted anywhere else, and a unit test asserts the password does not appear in the report |
| A plaintext password on the wire | `sslmode=require`/`verify-*` is **refused with a named message**; the default is an explicit, documented plaintext connection to a host you chose. A migration over an untrusted network is out of scope and says so rather than pretending |
| A hostile or broken server sending a huge length prefix | every length is bounded before it is allocated; a packet over a configured ceiling is an error naming the byte count, not an allocation |
| Panicking on malformed protocol bytes | the crate carries the same lints as the rest of the workspace: `deny(clippy::indexing_slicing / unwrap_used / expect_used / panic)`, `forbid(unsafe_code)` |
| Reading the source twice and getting two different databases | one repeatable-read snapshot for the whole run; the final `count(*)` is inside the same snapshot, so it verifies the copy rather than racing the application |
| A half-written `.rdb` where an application looks | staged under a name nothing opens; published by rename; refuses an existing destination |
| A hung server wedging the run | a connect timeout and a read timeout on the socket, both settable from the URL, both defaulted |

## Alternatives considered

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| **Use the `postgres` crate** (already an allowed dependency) and add `mysql` | fastest to write; battle-tested auth incl. TLS | pulls a Tokio runtime into a shipped binary whose RSS is a published number; `mysql` is a **new** third-party dependency needing a policy row; two very different code paths for the two servers; contradicts "neither engine links another database" for the tool people run first | **rejected** |
| **Shell out to `pg_dump` / `mysqldump` and replay the SQL** | no protocol code at all | requires the other database's tooling installed — the exact thing `docs/dependency-policy.md` says an implementation agent must not need; dump dialects are not this dialect; no verification is possible because there is no second reader | rejected |
| **Read a `pg_dump` custom-format file** | offline, no server needed | still needs the dump to have been taken; the custom format is undocumented and version-coupled; users would have to learn a two-step procedure | rejected as the *only* path; may be worth adding later as a file source alongside SQLite |
| **First-party wire clients in a new dependency-free crate** | zero new dependencies; usable from the CLI and MCP as well as the standalone binary; one shape for "etc." — a third server is a third module implementing `RemoteSource`; testable against a recorded-transcript fake server | ~2,000 lines of protocol code we own; `caching_sha2_password` full auth and TLS are out of reach | **chosen** |
| Put the code in `inillucent-migrate` | no new crate | drags the retrieval engine into the CLI, so G4 becomes impossible | rejected |

## Testing strategy

Functional and integration first, per the testing standard. Every new target gets a
`tests/selection.toml` row.

### Tier `unit` — `inillucent-remote`'s own `#[cfg(test)]`

Values, not the absence of a crash: MD5/SHA-1/HMAC/PBKDF2/base64 against published vectors; the full
SCRAM-SHA-256 exchange against RFC 5802's worked example, asserting the exact client proof; URL
parsing including percent-decoding, IPv6, defaults and **redaction**; the type map, asserting the
`OwnedDatum` and the declared type for each of ~30 source types; length-prefix bounds refusing an
over-large packet.

### Tier `engine` — `crates/inillucent-remote/tests/protocol.rs`

**A fake server, in-process, over a real loopback socket**, replaying byte transcripts recorded from
the real PostgreSQL 17 and MySQL servers this ticket migrates. It asserts the *client's* bytes too,
not only that the client survived the server's: a startup packet with the wrong length, or a SCRAM
proof computed from the wrong salt, fails here rather than at a customer's server. This is what makes
G5 true — a fresh clone with no database installed runs these.

Cases: trust auth; md5 auth; SCRAM auth including a server signature that does **not** verify (must
be refused, not accepted); an `ErrorResponse` surfacing its SQLSTATE; a multi-batch cursor;
`mysql_native_password`; `caching_sha2_password` fast path; `caching_sha2_password` full-auth request
(must produce the named refusal); a truncated stream mid-row (must error, not hang or panic).

### Tier `engine` — `crates/inillucent-migrate/tests/remote.rs`

The whole procedure against the fake server: describe → stage → copy → verify → publish, then
**reopen the published `.rdb` with the engine and read the rows back**, asserting values and types.
Plus: a destination that already exists is refused; a verification failure publishes nothing and
leaves the staging file and the report; the report contains no password.

### Tier `e2e` — `crates/inillucent-remote/tests/live_postgres.rs`, `live_mysql.rs`

Gated on `INILLUCENT_TEST_POSTGRES_URL` / `INILLUCENT_TEST_MYSQL_URL`; each prints `; skipping` when
unset so `--strict` counts them, and each declares `requires = ["postgres"]` / `["mysql"]` in
`selection.toml`. They create a scratch database, populate it through **the other engine's own
client** (`psql` / `mysql`) so the oracle is not this code, migrate it, and compare every table's
rows against what the other client reports.

### The acceptance run (evidence for G6, recorded in the ticket)

1. A real PostgreSQL 17 (`trust`, and a second instance configured `scram-sha-256`), a database of
   every type in the map, migrated, verified, and the rows read back out of the `.rdb`.
2. A real MySQL, same.
3. `target/debug/inillucent-testrun --changed`, and the full runner, both green.

## What ships alongside: the agent front door

`AGENTS.md` at the repository root — what this project is, the four programs, where the rules live
and which test enforces each one, the layering and dependency policy in five lines, how to run the
right tests, and the shape of a change that passes review here.

`agent-skills/` — task-shaped skills in the `SKILL.md` convention (frontmatter `name` + `description`,
then the body), each one a thing an agent is actually asked to do:

| skill | what it covers |
|---|---|
| `inillucent-quickstart` | install, create a database, run SQL, read the JSON output, exit codes |
| `inillucent-query` | querying and inspecting an existing `.rdb`; `describe`, `explain`, `dump` |
| `inillucent-migrate` | SQLite, PostgreSQL, MySQL and the legacy index — including this ticket's work |
| `inillucent-search` | the retrieval engine: `VECTOR(N)`, the HNSW index, `inillucent_search` |
| `inillucent-embed` | binding the driver from Rust, Python, Node, Go and PHP |
| `inillucent-mcp` | wiring `inillucent-mcp` into an agent, `--readonly` and `--root` |
| `inillucent-develop` | working *on* this repository: layering, dependency policy, the test runner, the command table, where a new test goes |
| `inillucent-troubleshoot` | the failures that have real causes — exit code 3, a locked database, a refused overwrite, a `requires` skip that looks like a pass |

The README gains a short "For agents" pointer next to the existing MCP block.
