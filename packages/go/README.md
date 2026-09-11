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
and the git tag that releases it carries the same prefix: `packages/go/v0.1.0`.
That prefix is Go's own rule for a nested module, not a choice made here.

The prefix belongs to the tag. The version you ask for is the plain version:

```sh
go get github.com/Black-Rainbow-Labs/Inillucent/packages/go@v0.1.0
```

`@packages/go/v0.1.0` is rejected - `invalid version: version
"packages/go/v0.1.0" invalid: disallowed version string`. `@latest` works too
and resolves to the newest prefixed tag.

## Running the tests

They skip when inillucent is not installed, rather than failing on a machine
that never had it:

```sh
go install ./cmd/inillucent-install && go test ./...
```
