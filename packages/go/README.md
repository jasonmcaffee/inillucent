# inillucent, from Go

```sh
# Get the binaries. inillucent is Rust, so `go install` cannot build it - what
# it can build is a small program that downloads the release for your machine,
# verifies its SHA-256 and puts the four programs in GOBIN.
go install github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent-install@latest
inillucent-install

# Then use it from Go.
go get github.com/Black-Rainbow-Labs/Inillucent/packages/go
```

```go
package main

import (
    "context"
    "fmt"

    "github.com/Black-Rainbow-Labs/Inillucent/packages/go"
)

func main() {
    ctx := context.Background()
    db := inillucent.Open("app.rdb")

    if err := db.Batch(ctx, `
        CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
        INSERT INTO people VALUES (1, 'Ada', 36), (2, 'Grace', 45)
    `); err != nil {
        panic(err)
    }

    result, err := db.Query(ctx, "SELECT name FROM people WHERE age > ?1", 40)
    if err != nil {
        panic(err)
    }
    for _, row := range result.Maps() {
        fmt.Println(row["name"])   // Grace
    }
}
```

## What this is

A wrapper over the `inillucent` command line, in pure Go with no cgo. Each call
is a process, so it is the right tool for the handful of calls a build step, a
migration or an agent harness makes, and the wrong tool for a loop over a
million rows.

**It is not an in-process driver, and saying so is better than shipping one that
has never been run.** A real binding goes through the C ABI in
[`inillucent_driver.h`](../../drivers/inillucent-driver-capi/include/inillucent_driver.h)
with cgo or with [purego](https://github.com/ebitengine/purego), and
[`drivers/README.md`](../../drivers/README.md) is written to be followed by
somebody doing exactly that: it names the five rules a binding has to get right,
the three ownership rules and nothing else, and the conformance suite that says
whether you got them right.

## Errors that are answers

```go
_, err := db.Query(ctx, "SELECT * FROM absent")
var refused *inillucent.Error
if errors.As(err, &refused) {
    switch refused.Status {
    case inillucent.StatusNotFound:    // no such table
    case inillucent.StatusUnsupported: // the engine has not built that construct
    case inillucent.StatusConstraint:  // a check, a unique index, a foreign key
    }
}

if inillucent.Unsupported(err) {
    // Do not offer that feature. Rewording the statement will not help.
}
```

`StatusUnsupported` is its own status rather than a flavour of syntax error, and
that distinction is the whole shape of this engine's driver: it is deliberately
incomplete in places and refuses what it has not built rather than answering it
wrongly. An application that can tell the two apart can say "this database
cannot do that yet" instead of "check your spelling".

## Versioning

The module is in a subdirectory, so its import path carries that subdirectory
and the git tag that releases it carries the same prefix: `packages/go/v0.1.2`.
That prefix is Go's own rule for a nested module, not a choice made here.

The prefix belongs to the tag. The version you ask for is the plain version, and
`@latest` is the one to write down because it cannot go stale and point at a
release that has been superseded:

```sh
go get github.com/Black-Rainbow-Labs/Inillucent/packages/go@latest
```

`@packages/go/v0.1.2` is rejected - `invalid version: version
"packages/go/v0.1.2" invalid: disallowed version string`.

**Do not install `@v0.1.0`.** Its command directory was `cmd/inillucent`, so it
built a program that had to overwrite itself to finish its own job; `go.mod`
carries the same warning.

## Running the tests

They skip when inillucent is not installed, rather than failing on a machine
that never had it:

```sh
go install ./cmd/inillucent-install && go test ./...
```

## The API

Every method this binding has. The worked example each one appears in is the link; nothing here is
a summary of a method that does not exist, because
`cargo test -p inillucent-compat --test documentation` reads this table and fails on a name the
binding source does not declare.

| what | one line |
|---|---|
| `Open(path)` | a `*DB` over a database file. |
| `DB.Query(ctx, sql, params...)` | run one `SELECT` and return a `Result`. |
| `DB.Exec(ctx, sql, params...)` | run one statement for its effect and return how many rows it changed. |
| `DB.Batch(ctx, sql)` | run several statements separated by semicolons. |
| `DB.Describe(ctx, table)` | one table's columns. |
| `DB.Tables(ctx)` | every table in the database. |
| `DB.Search(ctx, table, query, k)` | the k nearest rows of a retrieval index. |
| `DB.Run(ctx, command, args)` | any command of the command line, for the ones the methods above do not wrap. |
| `Result.Field(name, into)` | one column of the first row, decoded into a Go value. |
| `Result.Maps()` | every row as a map, for a caller that would rather not name columns. |
| `Version(ctx)` | the version of the program this package found. |
| `Unsupported(err)` | whether the failure was "this engine has not built that" rather than "you typed it wrong". |
| `Error` | one failure, with its status and its message. |
| `Args` | the arguments a `Run` takes, as a map. |

Every call goes through the command line, for the reason the npm package's table gives.
