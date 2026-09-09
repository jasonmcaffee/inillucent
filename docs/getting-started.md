# Getting started

Install inillucent, make a database, run SQL against it, and understand what the programs tell you
when something goes wrong.

If you would rather learn by working through examples, the tutorial at
[inillucent.com/docs](https://inillucent.com/docs) covers the same ground in 23 chapters with the
expected output printed beside every command.

## Install

```powershell
# Windows
irm https://raw.githubusercontent.com/jasonmcaffee/inillucent/main/packaging/install.ps1 | iex
```

```sh
# macOS and Linux
curl -fsSL https://raw.githubusercontent.com/jasonmcaffee/inillucent/main/packaging/install.sh | sh
```

Or through a package manager:

| | |
|---|---|
| **npm** | `npm install -g inillucent`, or `npx inillucent help` with nothing installed |
| **pip** | `pip install inillucent` — the wheel carries the four programs and an in process driver |
| **cargo** | `cargo install inillucent-cli` — builds from source, and the fallback on any platform with no prebuilt archive |
| **Homebrew** | `brew install jasonmcaffee/inillucent/inillucent` |
| **Go** | `go install github.com/jasonmcaffee/inillucent/packages/go/cmd/inillucent@latest` |
| **Composer** | `composer require jasonmcaffee/inillucent && vendor/bin/inillucent-install` |

Each of them installs the same four programs, and each downloader checks the release's published
SHA-256 before unpacking the archive.

There is no macOS archive yet, because every platform's archive is built on that platform. On macOS,
use `cargo install inillucent-cli`.

`packaging/README.md` is how a release is cut. `packaging/windows/README.md` and
`packaging/macos/README.md` record what a signed installer would take on each platform and what it
would cost.

## The four programs

| program | what it is |
|---|---|
| `inillucent` | the command line: 29 verbs — `query`, `exec`, `describe`, `import`, `export`, `search`, `explain`, `backup`, `migrate` and the rest |
| `inillucent-shell` | an interactive shell shaped like `sqlite3`, with 63 of its 65 dot commands and all 48 of its command line options |
| `inillucent-mcp` | 27 of the same commands served to an AI agent over MCP |
| `inillucent-migrate` | builds a database from a SQLite file, a running PostgreSQL or MySQL server, or a legacy retrieval index |

## A first database

```sh
inillucent create app.rdb
inillucent --db app.rdb exec "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)"
inillucent --db app.rdb exec "INSERT INTO note (body) VALUES (?1)" --params '["hello"]'
inillucent --db app.rdb query "SELECT * FROM note"
inillucent --db app.rdb describe note
inillucent help
inillucent help migrate      # one command, every parameter
```

Or interactively:

```sh
inillucent-shell app.rdb
```

The shell answers `.tables`, `.schema`, `.mode`, `.import`, `.dump`, `.expert` aside, and 62 more of
`sqlite3`'s dot commands. [SQL support](sql.md) lists the two it does not answer and why.

## Bind parameters, do not paste values

```sh
inillucent --db app.rdb exec "INSERT INTO note (body) VALUES (?1)" --params '["it'\''s fine"]'
```

`--params` takes a JSON array and binds `?1`, `?2` and so on in order. The quoting defect this avoids
is the same one in every language, and it is the most common way an application ends up with a broken
row or an injected statement.

## `--output json`

Every command takes `--output json`, and what comes back is the same object a language binding sees:

```sh
inillucent --db app.rdb query "SELECT * FROM note" --output json
```

```json
{
  "status": "ok",
  "columns": ["id", "body"],
  "rows": [[1, "hello"]],
  "total": 1
}
```

The values are typed rather than rendered as text, `total` is the true row count and does not change
when `--limit` does, and a failure carries the driver's own status name. Parse that rather than the
aligned table a terminal prints.

## The exit codes

| code | meaning |
|---|---|
| `0` | the command succeeded |
| `1` | the command failed — bad SQL, a constraint, a missing table, an I/O error |
| `2` | the command line could not be acted on at all |
| `3` | the engine has not built that construct yet |

**Code `3` is separate from code `1` on purpose.** A script can branch on "this engine has not built
that" without matching on the text of a message, and a caller that gets a `3` should stop rewording
its SQL, because rewording will not help. Over the driver and over MCP the same condition is the
status `unsupported`.

## Ask the engine what it can do

```sh
inillucent capabilities
```

Every row of that answer is checked against the running engine by a test, in both directions: a
capability the engine claims and then fails, and one it denies and then performs, each turn the build
red. That is a different thing from a feature list somebody wrote by hand and then forgot to update.

## Bringing a database in

```sh
inillucent migrate legacy.db                                --destination app.rdb
inillucent migrate "postgres://jason@127.0.0.1:5432/corpus" --destination corpus.rdb
inillucent migrate "mysql://root@127.0.0.1:3306/app"        --destination app.rdb
```

A path is a SQLite file. A `postgres://` or `mysql://` URL is a running server, read over its own
wire protocol. [Migrating](migrating.md) covers what is carried, what is only reported, and the two
authentication limits that are refused by name rather than worked around.

## Using it from an application

The driver is `drivers/inillucent-driver`, with a C ABI over it in
`drivers/inillucent-driver-capi`. `drivers/README.md` is the page for somebody writing a binding.
For Rust, the library crate is `inillucent`:

```rust
use inillucent::Database;

let database = Database::open("app.rdb")?;
let connection = database.connect()?;
connection.execute_batch("CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)")?;
let rows = connection.query("SELECT * FROM note", &[])?;
```

[`agent-skills/inillucent-embed`](../agent-skills/inillucent-embed/SKILL.md) covers Rust, Python,
Node, Go, PHP and C.

## Where to go next

- [SQL support](sql.md) — what runs, what differs from SQLite, what is refused
- [Vector search](vector-search.md) — `VECTOR(N)` columns, HNSW, and hybrid retrieval
- [Architecture](architecture.md) — how the retrieval engine works
- [Performance](performance.md) — the measurements against SQLite
