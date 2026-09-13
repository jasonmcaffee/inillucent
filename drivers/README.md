# The inillucent driver

What an application uses to talk to inillucent's relational engine, from Rust
and from every other language.

This directory is a sub project: it is the one part of the repository written to
be read by somebody who is **not** working on the engine. If you are writing a
binding, this file and
[`inillucent-driver-capi/include/inillucent_driver.h`](inillucent-driver-capi/include/inillucent_driver.h)
are what you need, and you should not have to read any Rust.


---

## Two requirements

This engine is incomplete in places. It refuses what it has not built, and it
says so with a status of its own. Two things follow, and a binding has to be
designed for both.

1. **Handle `unsupported`.** `unsupported` and `INILLUCENT_UNSUPPORTED` are the
   status for "this engine has not built that". It is a different status from
   the one a mistyped statement gets, so an application can tell a construct
   that does not exist yet from a construct it got wrong. A binding that folds
   this into its general error type cannot tell them apart. See
   [Writing a binding](#writing-a-binding), rule 4.
2. **Read the capability table before composing SQL.**
   `inillucent_capability()` enumerates what the engine does, with a sentence
   about each. A test checks every row against the running engine in both
   directions: a claim of support that fails turns the build red, and so does a
   claim of absence that now works. See
   [The capability table](#the-capability-table).

The engine reports `cancel` as partial support. A running statement stops at a
scan leaf or result batch, while an indivisible operator finishes before it can
observe the request:

```
$ python bindings/python/run_conformance.py
24 capabilities reported
```

---

## Layout

| | |
|---|---|
| `inillucent-driver/` | **the driver.** Rust, and it holds every decision. |
| `inillucent-driver-capi/` | the C ABI over it, as `cdylib` and `staticlib`. |
| `inillucent-driver-capi/include/inillucent_driver.h` | the contract a binding compiles against. |
| `abi.toml` | every C symbol, its stability and the version it appeared in. |
| `conformance/suite.json` | the driver's behaviour, as data. |
| `bindings/python/` | the reference binding, in the standard library only. |

`inillucent-driver` depends on `inillucent-engine` and nothing else in the
workspace, and `inillucent-driver-capi` depends on `inillucent-driver` and
nothing else. Those two edges are the point of the whole arrangement: the
engine's rearchitecture is still moving crates underneath, and a consumer
holding the driver is unaffected by all of it.

---

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

// A refusal you can act on. One arm covers the whole class, because every
// construct the engine has not built answers Unsupported rather than a syntax
// error.
match connection.query(statement, &[], 0) {
    Err(why) if why.status == Status::Unsupported => {
        println!("not yet: {}", why.feature.unwrap_or_default());
    }
    other => { other?; }
}
```

**No SQL statement answers `Unsupported` today.** The 416-case differential probe refuses nothing
SQLite answers. Window functions were the last twelve cases, and they answer now; `VACUUM`, which this
example used to name, rebuilds the file. Write the arm anyway. It is four lines, and a caller that
folds this status into its general error type has to be rewritten the first time a construct arrives
that does return it. `cancel` and `readonly_open` report partial support. Cancellation is observed
between executor units of work, not at every instruction. Read-only is enforced by this driver above
the engine, not by the file handle.

Rust does **not** go through the C ABI. DuckDB routes even its own first-party
Rust binding through its C API because its core is C++ and the ABI is the
narrowest thing it can be stable across; ours is a Rust core whose first
consumer is Rust, so doing the same would add a pointer round trip and a
`catch_unwind` per call, lose the type system across the seam, and put a Rust
caller's errors through a C integer and back, all of it to reach Rust.

### Six behaviours to design around

- **Values are typed, not text.** `Null | Integer | Real | Text | Blob`. A
  driver that rendered everything as text would be choosing a float's formatting
  on your behalf, and you would parse it back if you wanted the number.
- **`Null` is a variant, not an empty string.** They are different values and a
  layer that drew them the same is a layer nobody can trust.
- **`total` is exact.** The engine materialises, so a result arrives whole and
  the count was taken rather than estimated. `limit` caps the rows handed back;
  `total` and `more` describe what was produced. That is what lets a grid say
  `1–200 of 4,317` and mean it. The cost is that a query over a large table
  costs what the whole result costs. Put a `LIMIT` in your own SQL when you
  cannot afford that, where the planner can act on it.
- **A batch is one transaction, and the check happens before the commit.**
  `Connection::transaction(work, check)` takes the predicate as an argument on
  purpose: a postcondition tested after the `COMMIT` is a report about something
  that has already happened rather than a guard against it.
- **One file is one buffer pool**, and the engine is single threaded. A
  `Database` is neither `Send` nor `Sync`.
- **Read-only is enforced by the driver, not by the file.** A statement that
  does not bind to a query is refused. That is the binder's classification, not
  a scan of the text. The file is still open for writing, so the capability
  table says `partial` and says why.

---

## The capability table

```rust
for entry in inillucent_driver::CAPABILITIES {
    println!("{:24} {:8} {}", entry.name, entry.support.name(), entry.note);
}
if inillucent_driver::supports("cancel") != Some(Support::Yes) {
    // do not draw a Stop button
}
```

An unknown name answers `None` / `INILLUCENT_SUPPORT_UNKNOWN`, and you should
treat that as "no" rather than as "yes": a capability that was never declared
was certainly never checked.

### Why this one is worth trusting

`java.sql.DatabaseMetaData` has had `supportsFullOuterJoins()` since 1997 and
its answers are famously unreliable, because every driver hand-writes them and
nothing runs them. A capability list nobody checks decays into a list of claims
that were true once, and an application ends up refusing to offer something that
has worked for six months.

So `inillucent-driver/tests/capability.rs` runs every row against a real
database and fails in **both** directions:

- declared supported, probe fails → the driver was lying about something it
  offered;
- declared **unsupported, probe succeeds** → the engine has grown it and the
  table is stale, and the failure says so in as many words.

That second half has already earned its keep. Between this driver being written
and being finished, later work landed outer joins,
recursive CTEs, foreign keys, triggers, `ATTACH`, temporary tables, `STRICT` and
`ALTER TABLE ADD COLUMN … DEFAULT`. Every one of those turned the test red with
a message naming the row to update, rather than leaving an application quietly
refusing to offer something that works.

**A row's probe is not optional.** Two rows once carried `Probe::Nothing`, on
the grounds that the driver had no call for registering a function. The engine grew
`create_scalar_function` underneath them, so they rotted invisibly, exactly the
way a JDBC list rots. They now register a doubling function and a
case-insensitive collation and then **call them from SQL**, because a registry
no statement can reach is precisely the failure that row used to describe.

---

## The conformance suite

`conformance/suite.json` is the driver's behaviour written as data rather than
as prose, because a specification nobody can run is a specification every
implementation reads differently.

Two runners exist and both read that one file:

```bash
# Rust
cargo test --manifest-path <repo>/Cargo.toml -p inillucent-driver --test conformance

# Python, through the C ABI
cargo build --manifest-path <repo>/Cargo.toml -p inillucent-driver-capi
python drivers/bindings/python/run_conformance.py
```

When they disagree, one of the bindings is wrong. When they agree, the
specification is followable. That is a claim about this README, and the only
way to test it is to have somebody follow it in a second language.

The file's own `about` section documents its shape. In short: a case has
`setup` statements and `steps`, a step asserts only the keys it carries, and a
value is a one-key object (`{"int": 7}`, `{"text": "one"}`, `{"null": true}`,
`{"blob": [0, 255]}`) so that NULL and the empty string can never be confused by
the file itself.

**Add a case whenever you fix something.** The `returning_gives_rows_and_a_count`
case exists because writing it found that `INSERT … RETURNING a, b` produced its
rows with an empty column list: two columns of values and no headings for them.

---

## Writing a binding

The shape is the same in every language.

1. **Load the library and check `inillucent_abi_version()`'s major** against the
   one you were written for. Refuse a mismatch *by name*. The alternative is
   calling a function whose signature has moved, which does not fail in a way
   anybody can read.
2. **Wrap each opaque pointer in your language's own resource type**, with a
   finaliser calling the matching `_free`, and make the finaliser safe to run
   twice.
3. **After every call that takes an `inillucent_error **`, check the status
   first.** On non-zero, build your exception from `inillucent_error_status`,
   `_message`, `_feature` and `_offset`. Then call `inillucent_error_free` **in
   a `finally`**, because an exception constructed from the error must not leak
   it.
4. **Map `INILLUCENT_UNSUPPORTED` to its own exception type**, not to the
   general one. This is the whole of the design arriving in your language.
5. **Copy every `const char *` into your own string on the way out.** It points
   inside a handle the caller may free.
6. **Run `conformance/suite.json`.** A binding that has not run it is a binding
   that has not been tested.

### The three ownership rules, and there are no others

1. A handle named by a `_free` (or `_close`) is yours to free, exactly once.
   Freeing `NULL` is a no-op, so a finaliser need not check.
2. Every pointer the library **returns** points inside the handle you asked, is
   valid until that handle is freed, and is never freed by you. It does not
   move: a result is materialised, so a pointer into row 0 stays valid while row
   900,000 is read.
3. Every pointer you **pass in** is copied before the call returns. You may free
   your buffer on the next line.

Text is **not** NUL-terminated. A text value may contain a NUL byte, and
pretending otherwise would truncate it silently, so byte accessors take a
length out-parameter.

### Lifetimes in a garbage-collected language

A connection is only valid while its database is, and non-deterministic
finalisation will otherwise free them in the wrong order. Hold a strong
reference from child to parent: the connection object keeps the database alive,
the statement keeps the connection. A result keeps nothing, because it is
materialised and self-contained. That is deliberate: it is the handle most
likely to outlive its statement in a `for row in query(...)` idiom.

`inillucent_close` refuses with `INILLUCENT_INVALID_STATE` while any connection
is open rather than leaving them dangling, so getting this wrong is an error
rather than a crash.

### Threads

One file is one buffer pool and the engine is single threaded. Confine a
`inillucent_db` and everything under it to one thread, or serialise every call
on it with a lock your binding owns. **There is no lock inside**, and the driver
does not pretend there is. Two databases on two files are independent.

### Sessions, and a connection per call

A **session** is what `temp.`, `ATTACH` and the connection pragmas are scoped
to. `inillucent_connect` opens one and the handle keeps it, so every call on
that handle is the same session however the call is implemented underneath.
That matters, because underneath it is a connection per call: the Rust
`Connection` borrows its `Database`, and a C handle cannot hold a borrow.

This matters to a binding that does *not* go through
the C ABI. `inillucent_driver::Connection<'d>` borrows the `Database`, so a
long-lived object cannot hold both. It would be a self-referential struct, and
Rust will not have it. A caller in that position holds the `Database` and connects
per call, and must carry the session across:

```rust
let session = database.connect().session();
// ... later, and for every call:
let connection = database.connect_as(session);
```

Without it every call is a new session, and a `CREATE TEMP TABLE` typed into a
query console is gone by the next statement. `suite.json`'s
`a_temp_table_survives_a_connection_per_call` is the case for this, and it is
the one case that sets `"connection": "per_call"`.

### Known-good starting points

- **Python**: `ctypes`, standard library only. `bindings/python/inillucent.py`
  is the reference implementation and is written to be read.
- **Node**: `koffi`, or N-API if a compiled addon is acceptable.
- **Go**: `cgo`, or `purego` to avoid it.
- **Java**: the Foreign Function & Memory API on 22+, and JNI below that.
- **C#**: `DllImport` with `SafeHandle` subclasses, which give you the
  lifetime rule above for free.

---

## The ABI's stability

`abi.toml` gives every symbol a stability and a `since`, and
`inillucent-driver-capi/tests/abi.rs` checks that the header, the manifest and
the implementation all name the same set. It also checks that the header's
numeric constants are the driver's own enum values, because **the numbers are the ABI**
and a variant renumbered on the Rust side without the header changing would make
every binding in every language silently misread every error, with both sides
still compiling.

- **stable**: the signature will not change and the symbol will not be removed.
- **provisional**: it exists and may change in a minor version. Today that is
  `inillucent_cancel` alone. It requests cancellation of a running statement,
  which reports `INTERRUPTED` once the executor reaches a cancellation point.

Per-symbol rather than per-library is the shape DuckDB v2.0 moved its C API to
in August 2026, and it is the one thing from that design worth copying wholesale.

---

## Building

```bash
cargo build --manifest-path <repo>/Cargo.toml -p inillucent-driver-capi
```

produces, in `target/<profile>/`:

- `inillucent_driver_capi.dll` / `.so` / `.dylib`, which is what a binding loads;
- `inillucent_driver_capi.lib` / `libinillucent_driver_capi.a`, for a C or C++
  program that would rather link it.

The header has no dependencies beyond `stdint.h` and `stddef.h`.
