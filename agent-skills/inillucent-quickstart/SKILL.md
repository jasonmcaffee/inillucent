---
name: inillucent-quickstart
description: Install inillucent, create a database, run SQL against it, read its JSON output, and understand its exit codes. Use when starting with inillucent for the first time, when asked to "set up a database", or when you need the shortest correct path from nothing to a working .rdb.
---

# Getting started with inillucent

inillucent is an embedded SQL database. It speaks SQLite's dialect on its own storage, and it has
full-text and vector search built into that same SQL. There is no server: a database is one `.rdb`
file, and the programs below open it directly.

## Install

```powershell
irm https://inillucent.com/downloads/install.ps1 | iex   # Windows
```

```sh
curl -fsSL https://inillucent.com/downloads/install.sh | sh  # macOS, Linux
```

Or from Go, which is published —
`go install github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent-install@latest`, then run
`inillucent-install`. It needs `GOPRIVATE=github.com/Black-Rainbow-Labs/*` set, because the repository is
private.

`npm install -g inillucent`, `pip install inillucent`, `cargo install inillucent-cli`,
`brew install black-rainbow-labs/inillucent/inillucent` and `composer require black-rainbow-labs/inillucent`
are what the other five will be, and none of them answers yet. Use the two commands above meanwhile.
Every route installs the same four programs and verifies the release's published SHA-256 first.

From a clone: `cargo build --release -p inillucent-cli`, and the binaries land in `target/release`.

## The first five minutes

```sh
inillucent create app.rdb
inillucent --db app.rdb exec "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT, created TEXT)"
inillucent --db app.rdb exec "INSERT INTO note (body, created) VALUES (?1, ?2)" \
  --params '["first note", "2026-01-02"]'
inillucent --db app.rdb query "SELECT * FROM note"
inillucent --db app.rdb describe note
```

`--db` can also come from `$INILLUCENT_DB`. With neither, the database is `:memory:` — useful for a
scratch query, and gone when the process ends.

## The four things worth knowing before you write anything

### 1. Exit code 3 is not an error in your SQL

| code | means |
|---|---|
| 0 | it worked |
| 1 | it failed |
| 2 | the command line was not one anybody could act on |
| **3** | **the engine has not built that construct yet** |

Three is separate on purpose so a script can branch on "not yet" without matching on a message. Over
a language binding or MCP the same thing is the status `unsupported`. **Rewording your SQL will not
help** — pick another construct, or check `capabilities` first.

### 2. Ask what it can do, rather than finding out

```sh
inillucent capabilities                 # the whole table
inillucent capabilities triggers        # one row
```

Every row is checked against the running engine by a test **in both directions**: a claimed
capability that fails and a denied one that now works each turn the build red. A name that is not in
the table answers *no*, because a capability nobody declared was never checked.

### 3. `--output json` is the machine surface

```sh
inillucent --db app.rdb query "SELECT * FROM note" --output json
```

```json
{
  "ok": true,
  "command": "query",
  "columns": [{ "name": "id", "type": "integer" }, { "name": "body", "type": "text" }],
  "rows": [[1, "first note", "2026-01-02"]],
  "total": 1,
  "more": false,
  "elapsed_ms": 0.4
}
```

Same object every command produces, and the same one a language binding sees. `total` is the count
*before* `--limit`, so `more: true` tells you rows were cut rather than absent. On a failure the
object carries `ok: false` and the driver's own `status` name — `unsupported`, `syntax`,
`constraint`, `invalid_state`, … — which is what to branch on. `--json` is short for
`--output json`.

### 4. Bind values, never paste them

```sh
inillucent --db app.rdb exec "INSERT INTO note (body) VALUES (?1)" --params '["O'\''Brien"]'
```

`--params` is a JSON array bound to `?1`, `?2`, … **in order**. It is how you avoid every quoting
bug, and it is the same rule in every binding.

## What else is here

| | |
|---|---|
| `inillucent help` | all 30 commands |
| `inillucent help <command>` | one command, every parameter, what each is for |
| `inillucent-shell app.rdb` | the `sqlite3`-shaped REPL, with 63 of its dot commands |
| `inillucent-mcp --db app.rdb` | the same commands served to an agent — see the `inillucent-mcp` skill |

Next: [`inillucent-query`](../inillucent-query/SKILL.md) to explore a database,
[`inillucent-search`](../inillucent-search/SKILL.md) for full-text and vectors,
[`inillucent-embed`](../inillucent-embed/SKILL.md) to put it in an application,
[`inillucent-migrate`](../inillucent-migrate/SKILL.md) to bring existing data in.
