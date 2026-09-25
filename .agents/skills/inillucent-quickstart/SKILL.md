---
name: inillucent-quickstart
description: Install inillucent, create a database, run SQL against it, read its JSON output, and understand its exit codes. Use when starting with inillucent for the first time, when asked to "set up a database", or when you need the shortest correct path from nothing to a working .rdb file.
---

# Getting started with inillucent

inillucent is an embedded SQL database. It speaks SQLite's dialect on its own storage, and it has
keyword search and vector search in the same SQL. There is no server. A database is one `.rdb` file,
and the programs open that file directly.

## Install

```powershell
irm https://inillucent.com/downloads/install.ps1 | iex        # Windows
```

```sh
curl -fsSL https://inillucent.com/downloads/install.sh | sh   # macOS and Linux
```

Both scripts check the download against the published `SHA256SUMS`, install into your home folder,
and need no administrator rights.

Package managers install the same programs:

| Package manager | Command |
|---|---|
| Homebrew | `brew install black-rainbow-labs/inillucent/inillucent` |
| npm | `npm install -g inillucent` |
| pip | `pip install inillucent` |
| Go | `go install github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent-install@latest && inillucent-install` |
| Composer | `composer require black-rainbow-labs/inillucent && vendor/bin/inillucent-install` |
| cargo | `cargo install inillucent-cli inillucent-migrate` |

To build from a clone of the repository:

```sh
cargo build --release -p inillucent-cli -p inillucent-migrate
```

The programs are written to cargo's `release` folder. Add `--features inillucent-cli/embed` if you
want the `embed()` SQL function. The published releases are built with it.

## Your first database

```sh
inillucent create app.rdb
inillucent --db app.rdb exec "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT, created TEXT)"
inillucent --db app.rdb exec "INSERT INTO note (body, created) VALUES (?1, ?2)" \
  --params '["first note", "2026-01-02"]'
inillucent --db app.rdb query "SELECT * FROM note"
inillucent --db app.rdb describe note
```

`--db` can also come from the `INILLUCENT_DB` environment variable. With neither, the database is
`:memory:`. A `:memory:` database is discarded when the process ends.

## Four things to know before you write anything

### 1. Exit code 3 means "not built yet"

| Exit code | Meaning |
|---|---|
| 0 | the command worked |
| 1 | the command failed |
| 2 | the command line was wrong: an unknown command, a missing argument |
| **3** | **the engine has not built that feature** |

Exit code 3 is separate so a script can tell "not built yet" from "wrong" without reading the
message. Through a language binding or MCP, the same case has the status `unsupported`. Rewording
the SQL does not help. Use a different construct, or check `inillucent capabilities` first.

### 2. Ask what the engine can do

```sh
inillucent capabilities             # the whole table
inillucent capabilities triggers    # one row
```

A test checks the rows against the running engine in both directions. A row that says yes and
fails, or a row that says no and works, fails the build. Two rows cannot be checked that way,
`cancel` and `readonly_open`, and both say `partial`. A name that is not in the table is refused with
the status `not_found`. Treat an unknown name as no.

### 3. Read results as JSON

```sh
inillucent --db app.rdb query "SELECT * FROM note" --output json
```

```json
{
  "ok": true,
  "command": "query",
  "columns": [
    { "name": "id", "type": "integer" },
    { "name": "body", "type": "text" },
    { "name": "created", "type": "text" }
  ],
  "rows": [[1, "first note", "2026-01-02"]],
  "row_count": 1,
  "total": 1,
  "more": false,
  "changes": 0,
  "last_insert_rowid": 0,
  "elapsed_ms": 0.1128,
  "text": "id  body        created\n--  ----------  ----------\n1   first note  2026-01-02"
}
```

Every command writes this object, and a language binding sees the same fields.

| Field | Meaning |
|---|---|
| `ok` | `true` when the command worked |
| `columns` | each column's name and the storage class its values had: `integer`, `real`, `text`, `blob`, `null`, or `mixed` |
| `rows` | the rows, as arrays of typed values |
| `row_count` | how many rows are in `rows` |
| `total` | how many rows the query produced, before `--limit` cut the list |
| `more` | `true` when `--limit` cut rows from the list |
| `changes` | rows changed by an `exec` or `batch` |
| `last_insert_rowid` | the rowid of the last row inserted |
| `text` | the same result as the text table a person reads |

`--limit` defaults to 200 rows. `--limit 0` returns every row.

On a failure, `ok` is `false` and the object has a `status` and a `message`:

```text
{ "ok": false, "command": "query", "status": "not_found", "message": "no such table: nope", ... }
```

Branch on `status`. Its values include `unsupported`, `syntax`, `not_found`, `constraint`,
`readonly`, `busy` and `invalid_state`. `--json` is short for `--output json`.

### 4. Bind values with `--params`

```sh
inillucent --db app.rdb exec "INSERT INTO note (body) VALUES (?1)" --params '["O'\''Brien"]'
```

`--params` is a JSON array. Its values are bound to `?1`, `?2` and so on, in order. Binding avoids
quoting bugs, and every language binding works the same way.

## The other programs

| Command | What it does |
|---|---|
| `inillucent help` | lists all 30 commands |
| `inillucent help <command>` | explains one command and every parameter it takes |
| `inillucent-shell app.rdb` | an interactive shell that works like `sqlite3`, with 63 of its 65 dot commands |
| `inillucent-mcp --db app.rdb` | serves the commands to an AI agent; see the `inillucent-mcp` skill |

## Next

- [`inillucent-query`](../inillucent-query/SKILL.md): explore a database.
- [`inillucent-search`](../inillucent-search/SKILL.md): keyword and vector search.
- [`inillucent-embed`](../inillucent-embed/SKILL.md): use inillucent inside an application.
- [`inillucent-migrate`](../inillucent-migrate/SKILL.md): copy existing data in.
