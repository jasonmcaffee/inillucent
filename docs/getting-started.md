# Getting started

Install inillucent, make a database, run SQL against it, and understand what the programs tell you
when something goes wrong.

If you would rather learn by working through examples, the tutorial at
[inillucent.com/docs](https://inillucent.com/docs) covers the same ground in 24 chapters with the
expected output printed beside every command.

## Install

```powershell
# Windows
irm https://inillucent.com/downloads/install.ps1 | iex
```

```sh
# macOS and Linux
curl -fsSL https://inillucent.com/downloads/install.sh | sh
```

Or from Go:

```sh
go install github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent-install@latest
inillucent-install
```

`go install` resolves the module through `proxy.golang.org`, which clones the repository with no
credential. The proxy serves it: both `@latest` and `@v/list` answer 200 to a signed-out caller, and
`tools/check-public-urls.mjs` checks that on every `tools/validate` run. It answered 404 until
the repository was made public, which is why `packaging/PUBLISHING.md` carried the route as
tagged and uninstallable for three releases.

Four more package managers serve the current release:

| | |
|---|---|
| **npm** | `npm install -g inillucent`, or `npx inillucent help` with nothing installed |
| **pip** | `pip install inillucent` — the wheel carries the four programs and an in process driver |
| **Homebrew** | `brew install black-rainbow-labs/inillucent/inillucent` |
| **Composer** | `composer require black-rainbow-labs/inillucent && vendor/bin/inillucent-install` |

Each of them installs the same four programs, and each downloader checks the release's published
SHA-256 before unpacking the archive.

**cargo builds from the repository, not from crates.io.** crates.io carries five of the workspace's
library crates and not `inillucent-cli`, so `cargo install inillucent-cli` finds nothing. This
works on any platform with a Rust toolchain:

```sh
cargo install --git https://github.com/Black-Rainbow-Labs/Inillucent inillucent-cli
```

**macOS is a universal build**, for Apple silicon and Intel: a `.tar.gz` archive, and a `.pkg`
installer signed with a Developer ID and notarised by Apple. Both are on the GitHub release and on
inillucent.com.

`packaging/README.md` is how a release is cut. `packaging/windows/README.md` and
`packaging/macos/README.md` record what a signed installer would take on each platform and what it
would cost.

## The four programs

| program | what it is |
|---|---|
| `inillucent` | the command line: 30 verbs — `query`, `exec`, `describe`, `import`, `export`, `search`, `explain`, `backup`, `migrate`, `setup-embeddings` and the rest |
| `inillucent-shell` | an interactive shell shaped like `sqlite3`, with 63 of its 65 dot commands. It knows all 48 of `sqlite3`'s command line options: 30 it acts on, and 18 it refuses by name because this engine has no equivalent |
| `inillucent-mcp` | 28 of the same commands served to an AI agent over MCP |
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

The shell answers 63 of `sqlite3`'s 65 dot commands, `.tables`, `.schema`, `.mode`, `.import` and
`.dump` among them. The two it does not answer are `.expert` and `.session`; [SQL support](sql.md)
says why.

## A first word that is not a command

`inillucent` reads its first word as a command. When the command table does not have that word, it is
read as a database to open in the shell instead, so `inillucent app.rdb "SELECT 1"` works the way
`sqlite3 app.rdb "SELECT 1"` does. The file is created if it is not there yet, so a word read as a
database is a file written to disk.

So a word is read as a database only when it could be a file name.

| the first word | what it opens |
|---|---|
| `:memory:` | a database held in memory |
| `file:app.rdb?mode=ro` | the database the URI names |
| a word that is already a file on disk | that file |
| a word with a separator in it, such as `./ledger` or `data/app` | that path |
| a word with a drive letter in front of it, such as `C:\tmp\app` | that path |
| a word with an extension on the end, such as `app.rdb` | that file, created if it is not there |
| anything else | nothing: it is refused |

Anything else is a mistyped command. It is refused before anything is opened, with exit code 2:

```text
inillucent: 'qeury' is not a command, and it does not name a database file.
  Did you mean: query?
  Run 'inillucent help' for the 30 commands there are.
  To open a file of that name as a database, write it as a path: inillucent ./qeury
```

To open a file whose name has no extension, write it as a path: `inillucent ./ledger "SELECT 1"`.

`inillucent-shell` applies the same reasoning to its options. A word that begins with a dash and is
not one of the options it knows is refused rather than taken for the file name, which is what
`sqlite3` itself does:

```text
Error: unknown option: --db
Use -help for a list of options.
```

To open a file whose name begins with a dash, put `--` in front of it, as in
`inillucent-shell -- -ledger.rdb`.

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
  "ok": true,
  "command": "query",
  "columns": [
    {
      "name": "id",
      "type": "integer"
    },
    {
      "name": "body",
      "type": "text"
    }
  ],
  "rows": [
    [
      1,
      "hello"
    ]
  ],
  "row_count": 1,
  "total": 1,
  "more": false,
  "changes": 0,
  "last_insert_rowid": 0,
  "elapsed_ms": 0.42,
  "text": "id  body
--  -----
1   hello"
}
```

`elapsed_ms` is whatever the statement took, so it differs on every run; everything else is what the
command above prints. Each column carries its name and the storage class its values came back as.
The values are typed rather than rendered as text, `total` is the true row count and does not change
when `--limit` does, `more` says whether a `--limit` cut the answer short, and a failure carries the
driver's own status name in place of `ok`. Parse that rather than the aligned table a terminal
prints, which is also in the object as `text`.

A blob comes back as `{"blob":"<hex>"}` and its column type is `blob`. That is also how `--params`
takes one, so bytes read out of one result bind into the next statement unchanged:

```sh
inillucent --db app.rdb exec "INSERT INTO file (data) VALUES (?1)" --params '[{"blob":"00ff7f80"}]'
```

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
inillucent migrate "postgres://user@127.0.0.1:5432/corpus" --destination corpus.rdb
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
