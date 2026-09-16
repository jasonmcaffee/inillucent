---
name: inillucent-troubleshoot
description: Diagnose an inillucent failure - exit code 3 and the unsupported status, a refused overwrite, a locked or busy database, a query that is slower than expected, a test suite that is green because its prerequisite is missing. Use when inillucent did something unexpected, an error message needs interpreting, or a green result looks too easy.
---

# When inillucent does something you did not expect

Start by reading the *class* of the failure rather than the sentence. Every surface reports one:
the process exit code, or the `status` field in `--output json` / a driver error / an MCP result.

| exit | status | means |
|---|---|---|
| 0 | — | it worked |
| 1 | `syntax`, `constraint`, `io`, `invalid_state`, `not_found`, … | it failed, and the status says how |
| 2 | — | the command line was not one anybody could act on |
| **3** | **`unsupported`** | **the engine has not built that construct** |

## "It says unsupported / it exited 3"

**This is not a mistake in your SQL, and rewording will not help.** Three is a separate code on
purpose so a script can branch on "not yet" without matching on a message.

```sh
inillucent capabilities                # the whole table
inillucent capabilities triggers       # one row
```

Every row is checked against the running engine by a test in **both** directions — a claimed
capability that fails and a denied one that now works each turn the build red — so it is worth
trusting in a way a hand-written feature list is not. **A name that is not in the table answers
*no***, because a capability nobody declared was never checked.

`docs/feature-comparison.md` is the measured side-by-side against SQLite: 416 differential cases, 403 of
which produce SQLite's exact bytes, and every one of the differences named with what it measures. Six
of the other thirteen are vector features SQLite does not have and seven answer differently.

## "It refuses to write the file"

- **`… already exists. This tool never overwrites.`** — `create`, `migrate` and `backup` all refuse
  an existing destination rather than replacing it. Choose another path; the file that is there is
  untouched.
- **`… is outside <DIR>, which this server is confined to.`** — the surface was started with
  `--root`, and the path you named is not under it.
- **`… resolves to <PATH>, which is outside <DIR>, which this server is confined to.`** — the path
  you named *is* under `--root` and the file is not. Something on the way is a Windows junction or a
  Unix symbolic link pointing out of the root. The message names where it actually lands, which is
  what tells a junction apart from a typo.
- **`this surface is confined to a directory with --root, and a migration from a server reaches a
  host and a port`** — `--root` is about reach, not only about paths. Run the remote migration from
  an unconfined command line.
- **A statement was refused on a `--readonly` surface** — the classification is the binder's, so a
  `SELECT` containing the word "delete" is fine and `SELECT …; DROP TABLE …` is not.

## "The database is busy / locked"

One writer at a time, with a `busy_timeout`. A second writer waits and then reports busy rather than
corrupting anything. If a process is holding a write transaction open, that is the one to find. Note
that a `Database` is **single threaded** and neither `Send` nor `Sync` — sharing one across threads
is a different bug that will surface here.

## "A query is slower than I expected"

```sh
inillucent --db app.rdb explain "SELECT …"
```

A `SCAN` where you expected a `SEARCH` means the index is not being used, or is not there —
`describe <table>` lists the indexes that exist. `analyze` gathers the statistics the planner reads.

Two shapes worth knowing:

- **A result arrives whole.** `total` is exact because the engine materialises, so a query over a
  large table costs what the whole result costs. `--limit` caps what you are *handed*, not what was
  produced — put a `LIMIT` in your own SQL, where the planner can act on it.
- **Vector search with no index is an exhaustive scan**, and is still correct.
  `CREATE INDEX … USING inillucent_hnsw (v)` backfills the rows already there.

## "A value came back with the wrong type"

Values are dynamically typed, as in SQLite: a column's declared type is an **affinity** — a rule
about what converts on the way in — not a guarantee about what came out. `--output json` reports the
storage class each column's values *actually* had, and `mixed` when they disagreed. That is the fact;
the declared type in `describe` is the hint.

If the value arrived through a migration, check how it was carried: `numeric`/`decimal` and
`BIGINT UNSIGNED` past the signed range are carried as **TEXT**, digit for digit, on purpose — see
[`inillucent-migrate`](../inillucent-migrate/SKILL.md).

## "A migration failed"

Every check is printed whether it passed or not, so the output names which table and whether it was
the count or the digest that disagreed. Nothing that failed a check was published, and the staging
file and a `.migration-report.md` are left beside the destination — that is the evidence, not
litter. A leftover staging file makes the next run refuse rather than resume, because a server
changes underneath a resumed migration.

Login failures carry the server's own code: `postgres 28P01: …`, `mysql 1045 (28000): …`. Match on
the code, not on the sentence — the sentence follows the server's locale.

## "The test suite is green and I do not believe it"

Good instinct. Several suites need something the workspace cannot build — the pinned SQLite oracle, a
fixture corpus, a live PostgreSQL or MySQL — and each **reports success when it is absent**.

```sh
target/debug/inillucent-testrun --strict
```

`--strict` counts those and names them, so a green on a bare machine cannot be mistaken for a real
one. Set up the prerequisites first: `tools/sqlite-reference.ps1` (or `.sh`) for the oracle, and the
headers of `crates/inillucent-remote/tests/live_postgres.rs` and `live_mysql.rs` for the two servers.

## Still stuck

| | |
|---|---|
| what the engine gets wrong, and what it refuses | `README.md`, "What it gets wrong" |
| every construct, measured against SQLite | `docs/feature-comparison.md` |
| what a driver promises | `drivers/README.md` |
| how the suite decides what to run | `tests/inillucent-testing-tdd.md` |
