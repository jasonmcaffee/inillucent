---
name: inillucent-embed
description: Put inillucent inside an application - the Rust driver, the C ABI, and the Python, Node, Go and PHP bindings over it. Use when asked to add inillucent to a codebase, write or fix a language binding, or replace an existing SQLite/embedded-database dependency in an app.
---

# Embedding inillucent in an application

`drivers/README.md` is the full contract and is written for somebody who is **not** working on the
engine. This page is the shape of it, plus the parts that catch people out.

## The one thing to design for

**This engine refuses what it has not built, rather than answering it wrongly**, and there is a
status of its own for that: `unsupported` / `INILLUCENT_UNSUPPORTED` / exit code 3. It is **not** the
status a mistyped statement gets, and folding it into your general error type throws the design
away — an application needs to be able to say "this engine cannot do that yet" rather than "check
your spelling".

There is also a **capability table** you can ask *before* composing a statement, and it is checked
against the running engine by a test in both directions — a claimed capability that fails and a
denied one that now works each turn the build red. An unknown name answers "unknown", and you should
treat that as *no*: a capability nobody declared was never checked.

## Rust

```rust
use inillucent_driver::{Database, Status, Value};

let database = Database::open("app.rdb")?;
let connection = database.connect();

connection.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)", &[])?;
connection.execute(
    "INSERT INTO people VALUES (?1, ?2)",
    &[Value::Integer(1), Value::Text("Ada".into())],
)?;

let rows = connection.query("SELECT id, name FROM people", &[], 200)?;
println!("{} of {}{}", rows.rows.len(), rows.total, if rows.more { "+" } else { "" });

// Every statement this engine has not built answers Status::Unsupported rather
// than a syntax error, so one arm handles the whole class. Ask the capability
// table first if you want to know before you compose the statement.
match connection.query(statement, &[], 0) {
    Err(why) if why.status == Status::Unsupported => {
        println!("not yet: {}", why.feature.unwrap_or_default());
    }
    other => { other?; }
}
```

No SQL statement answers `Unsupported` today: the 416-case probe refuses nothing SQLite answers.
Write the arm anyway. It costs four lines, and a caller that folds this status into a general error
type has to be rewritten the first time a construct arrives that does return it.

Rust does **not** go through the C ABI — the core is Rust and its first consumer is Rust, so a
pointer round trip and a `catch_unwind` per call would buy nothing.

## Everything else

`packages/` ships the binaries and, for Python, an in-process driver:

```sh
pip install inillucent          # the wheel carries the binaries and the driver
npm install inillucent
go get github.com/Black-Rainbow-Labs/Inillucent/packages/go
composer require black-rainbow-labs/inillucent
```

For a language with no package here, bind the C ABI:
`drivers/inillucent-driver-capi/include/inillucent_driver.h` is the contract, `abi.toml` records every
symbol with its stability and the version it appeared in, and
`drivers/bindings/python/inillucent.py` is the reference binding — standard library only, written to
prove the header plus the README are enough.

## Six things that will otherwise cost you an afternoon

1. **Values are typed, not text.** `Null | Integer | Real | Text | Blob`. A driver that rendered
   everything as text would be choosing a float's formatting for you.
2. **`Null` is a variant, not an empty string.** They are different values.
3. **`total` is exact, and `limit` only caps what you are handed.** A result arrives whole, so
   `1–200 of 4,317` is a fact rather than an estimate. The cost is that a query over a large table
   costs what the whole result costs — put a `LIMIT` in *your own SQL*, where the planner can act on
   it, when you cannot afford that.
4. **A batch is one transaction, and the check runs before the commit.**
   `Connection::transaction(work, check)` takes the predicate as an argument on purpose: a
   postcondition tested after `COMMIT` is a report, not a guard.
5. **One file is one buffer pool, and the engine is single threaded.** A `Database` is neither `Send`
   nor `Sync`. Give each thread its own, or funnel through one.
6. **Read-only is enforced by the driver, not by the file.** A statement that does not bind to a
   query is refused — the binder's classification, not a scan of the text — but the file is still
   open for writing. The capability table says `partial`, and says why.

## Writing a binding

Read `drivers/README.md` §"Writing a binding" in full; it is short and each rule exists because
something broke. The three that matter most:

- **The three ownership rules, and there are no others** — the README states them; a binding that
  invents a fourth is leaking or double-freeing.
- **Keep `unsupported` distinct** in whatever your language's error type is.
- **Run the conformance suite.** `conformance/suite.json` is the driver's behaviour as data, and
  `drivers/bindings/python/run_conformance.py` is a worked runner over it. A binding that passes it
  agrees with the driver about every status, value and lifetime; one that does not, does not, and
  you will find out from a user instead.

## Instead of embedding

If the consumer is a person or an agent rather than a program, the command line already produces the
same object a binding sees:

```sh
inillucent --db app.rdb query "SELECT * FROM note" --output json
```

and `inillucent-mcp` serves 27 of those commands over MCP — see
[`inillucent-mcp`](../inillucent-mcp/SKILL.md).
