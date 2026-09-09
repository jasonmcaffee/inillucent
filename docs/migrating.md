# Migrating into inillucent

```sh
inillucent migrate legacy.db                                --destination app.rdb
inillucent migrate "postgres://jason@127.0.0.1:5432/corpus" --destination corpus.rdb
inillucent migrate "mysql://root@127.0.0.1:3306/app"        --destination app.rdb
```

A path is a SQLite file. A `postgres://` or `mysql://` URL is a **running server**, read over its own
wire protocol. `--kind postgres|mysql` overrides what the scheme says.

**[`agent-skills/inillucent-migrate`](../agent-skills/inillucent-migrate/SKILL.md) is the operator's
page.** It covers every source, what is carried, what is only reported, how each value type is
mapped, and how to read the report. This page is the summary and the reasoning.

## Three guarantees

These are why a migration is worth running through this rather than through a script.

**The source is never written to.** There is no flag that changes that and no cleanup step that
deletes anything. Going back means opening the file or the server that has been sitting there
untouched.

**The destination is never overwritten.** The build goes to a staging file beside it and is published
by an atomic rename, so a half written database never sits where an application opens one.

**Nothing unverified is published.** Every table is checked by row count *and* by an order
independent digest, because a migration that moved the right number of rows and the wrong bytes
passes a count check on its own. A failure publishes nothing, and leaves the staging file and a
written report — the evidence is the thing you need after a failed migration.

## From a running server, one instant

The whole read happens inside **one repeatable read, read only snapshot**, opened before the catalog
is read. Every table, and the schema itself, is as of one instant. Reading table B after table A
committed would produce a destination describing a database that never existed.

Rows stream through a cursor, so a table larger than memory costs one batch. `--batch N` sets how
many rows go into one destination transaction, 10,000 by default; it changes how long the migration
takes and nothing about what it produces.

The password comes from the URL, or from `PGPASSWORD` or `MYSQL_PWD`, and **it is redacted everywhere
it is printed** — the terminal, the written report, and the MCP result — because all three outlive
the run.

## Values that have no equivalent are carried as the server's own text

A `numeric(38,10)` rounded into an IEEE double is still a number, still eight bytes, and no check
anywhere would notice it had lost digits. So it is carried as `TEXT`, digit for digit, exactly what
`psql` prints. If you want it as a float in the destination, cast it there, knowingly.

The same applies to dates, times, `uuid`, `json`, arrays, ranges and enums. The full mapping table is
in the skill.

## The wire clients are first party

`crates/inillucent-remote` speaks the PostgreSQL and MySQL wire protocols itself. It depends on four
crates in this workspace and nothing else.

That is why `migrate --kind postgres` is reachable from the shipped command line and from MCP rather
than only from a separate tool: a production crate here may not link another database engine, so
pulling in the `postgres` and `mysql` crates would have put the feature behind a build flag or into a
side project. [Dependency policy](dependency-policy.md) has the argument in full, and it is worked
through as an example there.

## Two limits, refused by name rather than worked around

**No TLS.** `sslmode=require` is refused with a message saying so. Run the migration from a host you
trust the network to: a loopback address, or the database's own machine.

**MySQL 8's `caching_sha2_password` full authentication**, on an account the server's cache does not
hold, needs an RSA or TLS exchange this client does not speak. The refusal names the two ways out:
connect once with the `mysql` client to prime the cache, or run the migration as an account created
`IDENTIFIED WITH mysql_native_password`. Both are tested.

## What verification proves, and what it does not

The source's rows are read off a socket by one reader and the destination's off the leaves by
another, so a disagreement means the copy is wrong, and agreement means every value read reached the
destination unchanged.

What it does **not** prove is that the wire decoding was right in the first place, because the copy
and the digest share that reader. The oracle for that is a third engine — `psql` and `mysql`
themselves — and it lives in `crates/inillucent-remote/tests/live_postgres.rs` and `live_mysql.rs`.

## From SQLite

Tables, indexes, views and triggers come across. FTS5 tables are **rebuilt** rather than copied,
because their storage is SQLite's own: the text is indexed again through this engine's FTS5, and the
document ids are preserved. A virtual table using any other module is reported as not carried, with
its module named.

## Where to go next

- [`agent-skills/inillucent-migrate`](../agent-skills/inillucent-migrate/SKILL.md) — the full operator's page
- [Getting started](getting-started.md) — running the result
- [SQL support](sql.md) — what the destination can do with it
- [Removing PostgreSQL from a 5.8 GB Gmail assistant](real-world-use-cases/nikaya-postgres-to-inillucent.md) — a real migration, including what went wrong
