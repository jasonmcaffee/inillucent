# inillucent, from Go

inillucent is an embedded SQL database. It speaks SQLite's dialect, and it has keyword search and
vector search built in. One `.rdb` file holds the tables and the search indexes.

This Go package runs the `inillucent` command line program and returns its results as Go values. It
is pure Go and needs no cgo.

## Install

inillucent is written in Rust, so `go install` cannot build the database itself. Install the programs
first, then add the package to your module.

```sh
go install github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent-install@latest
inillucent-install

go get github.com/Black-Rainbow-Labs/Inillucent/packages/go@latest
```

`inillucent-install` downloads the release archive for your machine from
`https://inillucent.com/downloads`. It checks the archive's `SHA-256` against the release's
published `SHA256SUMS` file. Then it puts four programs in `GOBIN`: `inillucent`, `inillucent-shell`,
`inillucent-mcp` and `inillucent-migrate`. When `GOBIN` is not set, `inillucent-install` uses
`GOPATH/bin`, then `~/go/bin`.

| `inillucent-install` flag | What it does |
|---|---|
| none | installs the release this installer was built with, which has the same version as the Go module |
| `-version <version>` | installs that version, such as `-version 1.0.28` |
| `-version latest` | installs the newest release on the server |
| `-dir ./bin` | installs into `./bin` |

`inillucent-install` has builds for Windows on x64, Linux on x64 and arm64, and macOS on Apple
silicon and Intel.

If the programs are already installed some other way (Homebrew, npm, pip, the installer script),
skip `inillucent-install`. The package finds `inillucent` on `PATH`. Set `INILLUCENT_BIN` to use a
particular binary.

## A first program

```go
package main

import (
    "context"
    "fmt"

    inillucent "github.com/Black-Rainbow-Labs/Inillucent/packages/go"
)

func main() {
    ctx := context.Background()
    db := inillucent.Open("app.rdb")

    err := db.Batch(ctx, `
        CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
        INSERT INTO people VALUES (1, 'Ada', 36), (2, 'Grace', 45)
    `)
    if err != nil {
        panic(err)
    }

    result, err := db.Query(ctx, "SELECT name FROM people WHERE age > ?1", 40)
    if err != nil {
        panic(err)
    }
    for _, row := range result.Maps() {
        fmt.Println(row["name"]) // Grace
    }
}
```

`Open` does no work. The first command that writes creates `app.rdb`. `Batch` runs its statements
as one transaction, so either every statement takes effect or none does.

`?1`, `?2` and so on are bound to the values after the SQL, in order. Bind values this way instead of
pasting them into the SQL text.

## How the package runs

Each method call starts one `inillucent` process with `--output json` and reads the JSON object it
prints. A `DB` holds no open file and no connection. A `DB` is safe to share between goroutines.

Starting a process costs time on every call. This package suits a build step, a migration or an
agent harness that makes a few calls. It does not suit a loop over a million rows. For that, write a
binding over the C library. The
[driver guide](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/drivers/README.md)
explains how, with cgo or with [purego](https://github.com/ebitengine/purego).

The `DB` struct has four fields you can set.

| Field | What it does |
|---|---|
| `Path` | the database file. Empty means a database in memory, which is gone when the process ends |
| `Binary` | the `inillucent` program to run. Empty means `INILLUCENT_BIN`, then `inillucent` on `PATH` |
| `ReadOnly` | adds `--readonly`, so every statement that changes data is refused |
| `Root` | adds `--root`, so every path outside that directory is refused |

## Errors

```go
_, err := db.Query(ctx, "SELECT * FROM absent")

var refused *inillucent.Error
if errors.As(err, &refused) {
    switch refused.Status {
    case inillucent.StatusNotFound: // no such table
    case inillucent.StatusUnsupported: // the engine has not built that feature
    case inillucent.StatusConstraint: // a CHECK, a UNIQUE index or a foreign key refused the row
    }
}

if inillucent.Unsupported(err) {
    // Do not offer this feature. Rewording the SQL will not help.
}
```

When the engine refuses a statement, the method returns an `*inillucent.Error`. `Error.Status` is one
of thirteen statuses. `Error.Message` is the text, `Error.Feature` names the missing feature when the
status is `unsupported`, and `Error.Command` is the command that ran.

`StatusUnsupported` means the engine has not built that feature. The SQL is not wrong, and a
different spelling of the same SQL fails the same way. The `inillucent` program exits with code 3 in
that case, and with code 1 for every other failure. `inillucent capabilities` lists what the engine
supports.

| Status constant | Status text |
|---|---|
| `StatusUnsupported` | `unsupported` |
| `StatusSyntax` | `syntax` |
| `StatusNotFound` | `not_found` |
| `StatusConstraint` | `constraint` |
| `StatusReadOnly` | `readonly` |
| `StatusBusy` | `busy` |
| `StatusInterrupted` | `interrupted` |
| `StatusCorrupt` | `corrupt` |
| `StatusIO` | `io` |
| `StatusFull` | `full` |
| `StatusTooBig` | `too_big` |
| `StatusInvalidState` | `invalid_state` |
| `StatusInternal` | `internal` |

If the `inillucent` program cannot be started at all, the method returns an ordinary Go error that
contains the program's error output.

## Versions

The module lives in the `packages/go` folder of the repository. Go's rule for a module in a folder is
that its git tags carry the folder name, so a release tag looks like `packages/go/v<version>`. You
still ask for the plain version:

```sh
go get github.com/Black-Rainbow-Labs/Inillucent/packages/go@latest
```

`go get ...@packages/go/v<version>` fails with `invalid version`. Do not install `v0.1.0`. Its installer
command was in `cmd/inillucent`, and it had to overwrite itself to finish.

## Tests

The tests skip when `inillucent` is not installed.

```sh
go install ./cmd/inillucent-install && go test ./...
```

## The API

`cargo test -p inillucent-compat --test tooling documentation::` reads this table and fails if a name in the
first column is not declared in `inillucent.go`.

| what | one line |
|---|---|
| `Open(path)` | returns a `*DB` for a database file. Does no work until the first call. |
| `DB.Query(ctx, sql, params...)` | runs one statement that returns rows, and returns a `Result`. |
| `DB.Exec(ctx, sql, params...)` | runs one statement for its effect, and returns the number of rows it changed as an `int64`. |
| `DB.Batch(ctx, sql)` | runs several statements separated by semicolons as one transaction. |
| `DB.Describe(ctx, table)` | returns one table's columns, indexes, `CREATE` statement and row count. |
| `DB.Tables(ctx)` | returns the tables and views in the database. |
| `DB.Search(ctx, table, query, k)` | runs a keyword search over an FTS5 or `inillucent_search` table and returns the best `k` rows. `k` of 0 means the default of 10. |
| `DB.Run(ctx, command, arguments)` | runs any `inillucent` command, for the commands the methods above do not wrap. |
| `Result.Field(name, into)` | decodes one member of the JSON result that `Result` has no field for, such as `ddl` from `describe`. |
| `Result.Maps()` | returns every row as a `map[string]any` keyed by column name. |
| `DecodeValue(value)` | turns one cell into its Go value. A BLOB arrives as `{"blob": "<hex>"}` and becomes a `[]byte`. |
| `Version(ctx)` | returns the version text the installed `inillucent` prints. Also a quick check that the program runs. |
| `Unsupported(err)` | reports whether `err` is an `*Error` with status `unsupported`. |
| `Error` | one refusal: `Status`, `Message`, `Feature` and `Command`. |
| `Status` | the type of the thirteen status constants. |
| `Result` | the JSON result: `Columns`, `Rows`, `RowCount`, `Total`, `More`, `Changes`, `LastInsertRowID`, `ElapsedMS`, `Text`. |
| `Column` | one result column: its `Name` and the `Type` its values had. |
| `Args` | the named arguments for `DB.Run`, as a map. `Args{"table": "people"}` becomes `--table people`. |

`DB.Query` returns at most 200 rows, the default of the `query` command. `Result.Total` counts every
row the statement produced, and `Result.More` is `true` when rows were left out. To get every row,
pass a limit of 0 through `DB.Run`:

```go
result, err := db.Run(ctx, "query", inillucent.Args{"sql": "SELECT * FROM people", "limit": 0})
```

`DB.Run` takes the same parameter names that `inillucent help <command>` lists. A `[]any` value is
sent as JSON, a `true` value becomes a bare flag, and a `false` value is left out:

```go
result, err := db.Run(ctx, "describe", inillucent.Args{"table": "people"})
var ddl string
_ = result.Field("ddl", &ddl)
```

## More

- [Getting started](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/getting-started.md)
- [SQL support](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/sql.md)
- [Vector and keyword search](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/vector-search.md)
- [Glossary](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/glossary.md)

MIT licence. Source: <https://github.com/Black-Rainbow-Labs/Inillucent>
