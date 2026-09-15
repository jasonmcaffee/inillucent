---
name: inillucent-migrate
description: Move an existing database into inillucent - a SQLite file, a running PostgreSQL or MySQL server, or a legacy retrieval index - verified by row count and digest and published only if every check passes. Use when asked to migrate, import, convert or move data into an .rdb, or to evaluate inillucent against data that already exists somewhere else.
---

# Migrating into inillucent

Four sources. All of them hold the same three invariants, and they are the reason to use this rather
than a script:

1. **The source is never written to.** There is no flag that changes it and no cleanup step that
   deletes anything. Going back is opening the thing that has been sitting there unchanged.
2. **The destination is never overwritten.** The build goes to a staging file beside it and is
   published by an atomic rename, so a half-written database never sits where an application opens
   one.
3. **Nothing unverified is published.** Every table is checked by row count *and* by an
   order-independent digest — a migration that moved the right number of rows and the wrong bytes
   passes a count check on its own. A failure publishes nothing and leaves the staging file and a
   written report, because the thing you need after a failed migration is the evidence.

## From a SQLite file

```sh
inillucent migrate legacy.db --destination app.rdb
```

Tables, indexes, views and triggers come across. FTS5 tables are **rebuilt** rather than copied —
their storage is SQLite's own, so the text is re-indexed through this engine's fts5 and the docids
are preserved. A virtual table using any other module is reported as not carried, with its module
named.

## From a running PostgreSQL or MySQL

```sh
inillucent migrate "postgres://user@127.0.0.1:5432/corpus" --destination corpus.rdb
inillucent migrate "mysql://root@127.0.0.1:3306/app"        --destination app.rdb
```

The kind is taken from the URL's scheme; `--kind postgres|mysql` overrides it. `--batch N` sets how
many rows go in one destination transaction (default 10,000) and changes how long it takes and
nothing about what it produces.

**The whole read happens inside one repeatable-read, read-only snapshot**, opened before the catalog
is read — so every table, and even the schema, is as of one instant. Reading table B after table A
committed would produce a destination describing a database that never existed. Rows stream through
a cursor, so a table larger than memory costs one batch.

The password comes from the URL, or from `PGPASSWORD` / `MYSQL_PWD`. **It is redacted everywhere it
is printed** — the terminal, the written report, and the MCP result — because those outlive the run.

### What comes across, and what is reported instead

| | |
|---|---|
| carried | every ordinary table, its columns in order, its nullability, and its primary key |
| reported, not carried | views, sequences, materialized views, foreign tables, triggers, routines — one line each in the report |
| PostgreSQL schemas | a schema other than `public` is folded into the name as `schema__table`, because this dialect has one namespace. Two same-named tables in two schemas do not collide |

### How values are carried

Exactly where this dialect has an equivalent, and **as the server's own text rendering where it does
not**. That second half is the important one:

| source | carried as |
|---|---|
| integers | `INTEGER`, exactly |
| `real`, `double`, `float` | `REAL` |
| `numeric` / `decimal`, and `BIGINT UNSIGNED` past the signed range | **`TEXT`, digit for digit** |
| `boolean`, `TINYINT(1)` | `INTEGER`, 0 or 1 |
| `bytea`, `BLOB`, `BINARY`, `VARBINARY` | `BLOB` |
| dates, times, `uuid`, `json`, arrays, ranges, enums, everything else | `TEXT`, exactly as the server prints it |

A `numeric(38,10)` rounded into an IEEE double is still a number, still eight bytes, and no check
anywhere would notice. Its digits, carried as text, are what `psql` prints and cannot lose anything
they had. If you want it as a float in the destination, cast it there, knowingly.

### The connection is encrypted and verified, or it does not happen

**A migration to anything that is not a loopback address uses TLS, with the certificate chain and
host name checked, and refuses rather than falling back** — whether or not the URL says anything
about transport.

| what you write | what happens |
|---|---|
| nothing about transport, host is not loopback | verified TLS |
| `sslmode=require` / `verify-full` / MySQL's `ssl-mode=REQUIRED` | verified TLS |
| nothing, host is `127.0.0.1`, `::1` or `localhost` | plaintext |
| `sslmode=disable` **and** `--insecure-plaintext` | plaintext |
| either of those two on its own | refused |
| `sslmode=prefer` or `allow` | refused |

Plaintext across a network needs both halves because either one alone is something people type
without meaning it. `prefer` is refused rather than implemented: it means "encrypt if the server
happens to allow it", which puts the answer in a server setting nobody in the migration can see.

The certificate is checked **before any credential is sent**, so a server that turns TLS down or
presents a bad certificate gets no user name, no database and no password. A private authority goes
in `sslrootcert=<file>` (or `ssl-ca=`), and naming one is stricter than the machine store: that
authority becomes the only trusted root for the connection.

The report beside the destination and the `transport` field of `--output json` say `verified-tls` or
`plaintext`, so what happened is in the artifact.

### The limit that is refused by name rather than worked around

- **`caching_sha2_password` full authentication** (MySQL 8's default plugin, on an account the
  server's cache does not hold) needs an RSA exchange this client does not speak. The refusal
  names the two ways out: connect once with the `mysql` client to prime the cache, or run the
  migration as an account created `IDENTIFIED WITH mysql_native_password`. Both are tested.

## From a legacy retrieval index

```sh
inillucent-migrate <source-index-dir> <destination.db> [--no-publish]
```

This one is resumable — it keeps a manifest and picks up from the last committed batch, because a
directory does not change underneath a resumed run. A **server** does, so a remote migration is one
pass and a leftover staging file is refused rather than resumed.

## Reading the result

Every check is printed whether it passed or not: knowing that the counts are right and one table's
digest is not is a different problem from knowing that nothing arrived.

```
postgres://user:***@127.0.0.1:5432/corpus -> corpus.rdb
PostgreSQL 17.2, 6 tables, 13 rows
  pass structure.integrity every tree walks in key order on a fresh open
  pass source.count.note 3 rows, counted separately from the scan
  pass count.note 3 rows
  pass digest.note 3b226c3862edea05bf0590bb25b713ec…
  …
published: corpus.rdb
```

A `.migration-report.md` is written beside the destination with the same content plus what was not
carried. `--output json` gives you `tables`, `checks` and `notCarried` as arrays.

**What the verification proves**: the source's rows are read off a socket by one reader and the
destination's off PAX leaves by another, so a disagreement means the copy is wrong and agreement
means every value read reached the destination unchanged. **What it does not prove**: that the wire
decoding was right in the first place, because the copy and the digest share that reader. That
oracle is a third engine — `psql` and `mysql` themselves — and it lives in
`crates/inillucent-remote/tests/live_postgres.rs` and `live_mysql.rs`.

## From an agent, over MCP

`inillucent_migrate` takes the same arguments. One thing to expect: **a server confined with
`--root DIR` refuses a `postgres://` or `mysql://` source**, because the confinement is about reach
rather than about paths and a verb that dials a host and port would go straight through it. Run a
remote migration from an unconfined command line.

## If it fails

- **`… already exists. This tool never overwrites.`** — pick another destination. The file that is
  there is untouched.
- **`… is left over from an earlier migration`** — a previous run failed and left its staging file,
  which is the evidence. Read the report beside it, then move the staging file aside and re-run.
- **`postgres 28P01: …` / `mysql 1045 (28000): …`** — the server refused the login, and the code in
  front is its own SQLSTATE. Match on that, not on the sentence.
- **Verification failed** — nothing was published; the report names which table and whether it was
  the count or the digest that disagreed.
