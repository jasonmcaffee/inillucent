---
name: inillucent-embed
description: Put inillucent inside an application. Covers the Rust driver, the C library, and the Python, Node, Go and PHP packages. Use when asked to add inillucent to a codebase, write or fix a language binding, or replace SQLite or another embedded database in an application.
---

# Embedding inillucent in an application

This page shows how an application uses inillucent as a library, and the behaviors that most often
surprise a new user. `drivers/README.md` in the repository, copied as `DRIVER.md` into every
release archive, has the full contract.

## Terms used on this page

| Term | Meaning |
|---|---|
| driver | the Rust crate `inillucent-driver`. Every language reaches the engine through it |
| C library | `inillucent-driver-capi`, a C interface over the driver. Its header is `drivers/inillucent-driver-capi/include/inillucent_driver.h` |
| binding | code in another language that calls the C library |
| capability table | the list of features the engine reports, each marked `yes`, `partial` or `no` |
| session | what `temp.` tables, `ATTACH` and connection pragmas belong to |

## How each language reaches the engine

| Language | Package | How it reaches the engine |
|---|---|---|
| Rust | `inillucent-driver` on crates.io | calls the driver directly |
| C and C++ | the C library | calls the C library |
| Python | `pip install inillucent` | the `Database` class calls the C library. `run()` and `query()` start the `inillucent` program |
| Node | `npm install inillucent` | starts `inillucent --output json` and parses the JSON |
| Go | `go get github.com/Black-Rainbow-Labs/Inillucent/packages/go` | starts `inillucent --output json` and parses the JSON |
| PHP | `composer require black-rainbow-labs/inillucent` | starts `inillucent --output json` and parses the JSON |

All four language packages are published at version 1.0.29. The Python wheel holds the C library
and the four programs, so it needs no compiler.

A package that starts the program gets JSON. A blob comes back as `{"blob": "<hex>"}`. An integer
larger than 2^53 is rounded by JavaScript's `JSON.parse`, and by Go's `encoding/json` unless the
decoder calls `UseNumber()`.

## Design for `unsupported`

The engine returns the status `unsupported` for a statement it has not built. The same status is
`Status::Unsupported` in Rust, `INILLUCENT_UNSUPPORTED` in C, and exit code 3 on the command line.
A mistyped statement gets a different status, such as `syntax`.

```sh
inillucent query "SELECT 1 LIMIT 1 + 1"
```

```
Error [unsupported]: the new engine's physical pass does not handle a LIMIT or OFFSET that is not a constant yet
  not built yet: a LIMIT or OFFSET that is not a constant
```

Keep `unsupported` separate from other errors in your code. An application can then tell its user
"this engine cannot do that yet" instead of "check your SQL". No case in the 416 case probe against
SQLite returns `unsupported`, and the capability table still lists 17 features as `no`, so write the
branch.

**Ask the capability table before you write unusual SQL.** `inillucent capabilities` prints it, and
the driver's `capability(name)` returns one row. A test checks each row against the running engine.
An unknown name returns nothing. Treat that as no.

## Rust

```rust
use inillucent_driver::{Database, Status, Value};

let database = Database::open("app.rdb")?;
let connection = database.session();

connection.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)", &[])?;
connection.execute(
    "INSERT INTO people VALUES (?1, ?2)",
    &[Value::Integer(1), Value::Text("Ada".into())],
)?;

let rows = connection.query("SELECT id, name FROM people", &[], 200)?;
println!("{} of {}{}", rows.rows.len(), rows.total, if rows.more { "+" } else { "" });

// `limit` is a count. query(.., 0) returns no rows, with `total` set to the real count.
// Use query_all to get every row.
let everything = connection.query_all("SELECT id, name FROM people", &[])?;

match connection.query_all(statement, &[]) {
    Err(why) if why.status == Status::Unsupported => {
        println!("not yet: {}", why.feature.unwrap_or_default());
    }
    other => { other?; }
}
```

`Database::session` returns a `Connection`. The older name `Database::connect` still works and is
deprecated. Rust calls the driver directly and does not go through the C library.

`embed(TEXT)` is compiled in only with the `embed` feature. Name it on the crate you depend on:
`inillucent = { version = "1.0", features = ["embed"] }`, or `features = ["embed"]` on
`inillucent-driver`. Releases up to 1.0.29 have no such feature on either crate. With those, add
`inillucent-engine = { version = "1.0.29", features = ["embed"] }` beside it. The model is loaded
when `embed(TEXT)` is first called, from the folder `inillucent setup-embeddings all` installs it in.

`Database::open` uses these defaults from `OpenOptions`:

| Option | Default |
|---|---|
| `create` | `true` |
| `read_only` | `false` |
| `cache_frames` | 4,096 frames, which is 128 MiB at the engine's 32 KiB page |
| `limits` | unbounded |

## Six behaviors to design around

| Behavior | What it means for your code |
|---|---|
| **Values are typed.** | A value is `Null`, `Integer`, `Real`, `Text` or `Blob`. The driver does not turn numbers into text. |
| **`Null` is its own value.** | `Value::Null` and an empty `Value::Text` are different values. |
| **`total` is exact.** | The engine builds the whole result before it returns. `limit` only caps the rows handed back, and `more` is true when rows were left out. A query over a large table costs the whole result, so put a `LIMIT` in the SQL when that matters. |
| **A batch is one transaction, checked before the commit.** | `Connection::transaction(work, check)` runs every statement, passes the changed row counts to `check`, and commits only when `check` returns `Ok`. Any failure rolls back every statement. |
| **One database runs one statement at a time.** | A `Database` is neither `Send` nor `Sync`. To use one database from several threads, open it with `SharedDatabase`, which runs each statement in turn on a thread of its own. |
| **Read only is enforced by the driver.** | With `OpenOptions::read_only` set, the driver refuses any statement that does not bind to a query, including a `PRAGMA`. The file is still open for writing, so the `readonly_open` capability says `partial`. |

## Writing a binding for another language

Bind the C library. These files are the contract:

| File | What it holds |
|---|---|
| `drivers/inillucent-driver-capi/include/inillucent_driver.h` | the header a binding compiles against |
| `drivers/abi.toml` | every C symbol, with its stability and the version it first appeared in |
| `drivers/conformance/suite.json` | the driver's expected behavior, written as test cases |
| `drivers/bindings/python/inillucent.py` | the reference binding. It uses only the Python standard library |
| `drivers/bindings/python/run_conformance.py` | a runner for `suite.json`, written in Python |

The rules a binding must follow:

- **The three ownership rules.** A handle with a `_free` or `_close` function is yours to free,
  once. A pointer the library returns points inside a handle, stays valid until that handle is
  freed, and is never freed by you. The library copies every pointer you pass in before the call
  returns.
- **Map `INILLUCENT_UNSUPPORTED` to its own error type.**
- **Keep each database and its handles on one thread,** or guard every call with a lock your
  binding owns. The C library has no lock inside it. `inillucent_cancel` is the one call that is
  safe from another thread.
- **Run `drivers/conformance/suite.json`.** A binding that passes it agrees with the driver on every
  status, value and lifetime.

`drivers/README.md` has the full list of steps, the status codes and the lifetime rules for a language with
a garbage collector.

## Using the command line instead

When the caller is a person or an agent, the command line returns the same result object a binding
sees:

```sh
inillucent --db app.rdb query "SELECT * FROM note" --output json
```

`inillucent` has all 30 commands, and `inillucent-mcp` serves 28 of those commands over MCP. See
[`inillucent-mcp`](../inillucent-mcp/SKILL.md).
