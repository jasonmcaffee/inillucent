# task-1837 — the inillucent driver

A technical design for the thing an application uses to talk to inillucent's
relational engine, in Rust first and in other languages after.

---

## 0. The one-paragraph version

inillucent is an **in-process** engine: there is no server, no port and no wire
protocol, and adding one to reach other languages would be adding a network hop
to a database whose whole argument is that it does not have one. So the driver
is a **library**, and the way a library reaches many languages is a **C ABI**.
This design therefore ships two things and one document: `inillucent-driver`,
the Rust surface, which is the driver and holds every decision; and
`inillucent-driver-capi`, a small, versioned, opaque-handle C ABI over it, which
holds no decisions at all and is what Python, Node, Go, Java, C# and Ruby bind
to. The document is §§6–10, which is written so a binding author never has to
read the Rust.

The design's one distinguishing feature is **§4, the capability surface**. This
engine is deliberately incomplete — it cannot enforce a foreign key, answer a
`LEFT JOIN`, run a recursive CTE, or register a user-defined function — and the
whole difficulty of putting a driver in front of it is that an application must
be able to find that out *by asking*, and must never find it out by getting a
wrong answer. Every other database driver in existence solves the easy half of
this (JDBC's `supportsFullOuterJoins()`, ODBC's `SQLGetInfo`); the half that
matters here is that the answer has to be **derived from the engine and checked
by a test**, or it becomes a list of claims that were true once.

---

## 1. What the ticket asks for, and what it does not

> We need a library/module/driver to allow apps in various languages to
> communicate with inillucent db. Do online research, create a tdd. Implement
> first in Rust, but ensure the tdd is well defined so we can implement in other
> languages. Have the driver be a sub project in inillucent.

So: research, a design, a Rust implementation, and a design specific enough that
somebody who has never read this repository can write the Python one.

**Not in scope.** Closing any of the engine gaps in §4.2. Eleven qualification
suites in `inillucent-compat` are deliberately red and name exactly those gaps;
`_agent_output/task-1834-phase5/README.md` §13 is the list and §14 is Phase 6's
plan for them. A driver that made one of them go away would be a driver that had
changed the engine, and this ticket does not.

---

## 2. The consumer, because it decides the shape

The first consumer is **`crates/unluminous-db`** in `C:/jason/dev/unluminous`,
whose database explorer is driver-per-engine: `src/source.rs` has
`pub enum Engine { Postgres, Sqlite }` and `src/engine.rs` has a matching
`pub enum Database`, with SQLite reached through `rusqlite` and PostgreSQL
through a hand-written wire client. task-1814 adds `Engine::Inillucent` beside
them, and this driver is what that arm calls.

That enum is a contract, and reading it is how the driver's surface was chosen
rather than invented. Every method on `unluminous_db::Database`:

| the consumer calls | inillucent can answer it with | verdict |
|---|---|---|
| `connect(source, password)` | `Database::open(path)` | yes; no password, a file |
| `version()` | a build constant | yes |
| `databases()` | one — the file | yes |
| `schemas()` | one — `main` | yes |
| `items(schema)` | `SELECT type, name FROM sqlite_schema` | yes — it is a registered table |
| `table(schema, name)` | `PRAGMA table_info(x)` | yes — honoured pragma |
| `ddl(schema, name, kind)` | `SELECT sql FROM sqlite_schema WHERE name = ?` | yes — the original text is kept |
| `query(sql, limit)` | `Connection::query` | yes |
| `run(sql, values, limit)` | `Connection::query_with(sql, params)` | yes |
| `write(&[(sql, values)])` | `BEGIN` / … / check / `COMMIT` / `ROLLBACK` | yes — §5.4 |
| `use_schema(schema)` | nothing to do | yes, trivially |
| `stopper()` → `stop()` | **nothing** | **no — §4.3** |

Eleven of twelve. The twelfth is the honest failure this design is built to
express, and §4.3 is why it is a named refusal rather than a button that does
nothing.

Three of the consumer's own rules travel into the driver, because they are about
correctness rather than about its user interface:

- **NULL and the empty string are different values.** `unluminous_db::Value` has
  a `Null` variant for exactly this reason. So does ours.
- **A row can only be changed if it can be addressed.** The consumer decides
  that from the primary key, or from SQLite's `rowid` where a table has one and
  has not shadowed the name. The driver's job is to report the key faithfully —
  `PRAGMA table_info`'s `pk` column, and whether the table is `WITHOUT ROWID` —
  and never to guess one.
- **`1–200 of 200+` has to be honest.** The consumer asks for `limit + 1` rows
  and cuts back, so that "more" means somebody counted. §5.3 is what this driver
  does about that, and it is not the same trick.

---

## 3. Research: how everybody else does this, and what we take

Six designs were read. Each is here for the one decision it settles.

### 3.1 SQLite — the C API as the universal joint

SQLite is the existence proof for the whole approach: one C library, and every
language on earth reaches it through FFI. Two things it does are worth copying
and one is not.

Copy: **the C API is the contract, and it is kept**. Deprecated interfaces stay
supported; the header a caller compiles against is the promise. Copy also the
**loadable-extension thunk**, `sqlite3_api_routines` — a struct of function
pointers handed to an extension so the extension binds to a *table* rather than
to symbol addresses, which is how a library changes shape without breaking a
binary compiled against it. We do not need extensions, but the pattern is the
cheapest known route to ABI stability and §7.4 uses it.

Do not copy: **the surface's size**. `sqlite3.h` is over 250 entry points, and
`crates/inillucent-capi` in this repository already tried to reproduce it —
5,330 lines against the *old* engine, and it is on Phase 6's deletion list. A
driver is not a compatibility layer. §7 is 31 functions.

### 3.2 DuckDB v2.0 — the versioned specification

DuckDB v2.0 (August 2026) revised its C API around a **versioned specification
expressed in YAML**, with every symbol tagged with its lifecycle and stability
guarantee, a large part of the surface marked stable and frozen, and a stable
ABI across versions so extensions no longer need rebuilding per release. Every
other official DuckDB binding — Go, Rust, Python, Java — sits on that C API
rather than on the C++ internals, and the C++ and Rust extension wrappers
explicitly "talk only to the stable C ABI, so the binary you ship stays
independent of DuckDB versions".

This is the single most important find, and it settles two things:

1. **The Rust driver is not a wrapper over the C ABI; the C ABI is a wrapper
   over the Rust driver.** DuckDB's shape — one core, a C ABI, and every binding
   including its own first-party ones on that ABI — is right for a project that
   ships a C++ core to a dozen ecosystems. Ours is a Rust core and its first
   consumer is Rust, so making Rust pay an FFI round trip to reach a Rust engine
   would be paying DuckDB's cost without DuckDB's reason. §6.1.
2. **Stability is declared per symbol, not per library.** §7.5 gives every entry
   point a stability tag and a since-version, in a table that is checked.

We do not adopt the YAML-plus-codegen machinery. It earns its keep across
DuckDB's number of bindings and releases; here it would be a build step in front
of 31 functions.

### 3.3 ADBC — the error and status model

Arrow Database Connectivity, specification 1.1.0, is the modern standard for
this shape: a driver manager loads a driver by name, the driver exposes
`AdbcDriverInit`, and the object model is **Database / Connection / Statement** —
a database holding shared state and owning an in-memory instance, a connection
being one logical connection, a statement holding the execution state of a
one-off or prepared query and invalidating its previous result set on reuse.

Its `AdbcError` carries a message, a vendor-specific code, a five-character
SQLSTATE, and a release callback. Its status codes are the part worth quoting,
because they are a closed set somebody thought hard about:

```
ADBC_STATUS_OK / UNKNOWN / NOT_IMPLEMENTED / NOT_FOUND / ALREADY_EXISTS /
INVALID_ARGUMENT / INVALID_STATE / INVALID_DATA / INTEGRITY / INTERNAL / IO /
CANCELLED / TIMEOUT / UNAUTHENTICATED / UNAUTHORIZED
```

`ADBC_STATUS_NOT_IMPLEMENTED` — *"the operation is not implemented or
supported"* — is a first-class member of that set, sitting beside
`INVALID_ARGUMENT` rather than being folded into it. That is precisely the
distinction §4 is about, made by a standard rather than by us, and §7.2's status
list is ADBC's with the members this engine cannot produce removed.

**We do not implement ADBC itself.** Its result sets are streams of Arrow
`ArrowArrayStream`s rather than rows, which is right for the analytical
workloads it targets and wrong for a database explorer that draws a grid, and
which would put an Arrow C data interface implementation between this engine and
its first consumer. §11 keeps an ADBC driver as a later, additive option: an
ADBC driver over `inillucent-driver` is a straightforward piece of work once
somebody wants Arrow, and nothing here forecloses it.

### 3.4 JDBC and ODBC — capability discovery is thirty years old

`java.sql.DatabaseMetaData` has `supportsOuterJoins()`,
`supportsFullOuterJoins()` and `supportsLimitedOuterJoins()` as three separate
questions, plus `supportsMinimumSQLGrammar` / `Core` / `Extended` for the three
ODBC grammar levels; ODBC's `SQLGetInfo` is the same idea with an integer key.
The industry's answer to "the engine underneath cannot do everything" is a
metadata interface an application interrogates before it composes a statement.

Two lessons, one positive and one cautionary:

- **Positive.** Asking is normal, expected, and the thing tools already do. A
  driver that publishes a capability list is not exotic; it is the convention.
- **Cautionary.** JDBC's answers are famously unreliable — every driver hand-
  writes them, nothing checks them, and a driver that returns `true` from
  `supportsFullOuterJoins()` and then fails the query is a common experience.
  A list of claims nobody verifies rots into a list of claims that were true
  once. §4.4 is the whole of our answer to that, and it is the design's most
  important test rather than a nicety.

### 3.5 libSQL and Turso — what a second protocol costs

libSQL runs in three modes: local (the C library), remote (a pure client
speaking **Hrana**, its own HTTP/WebSocket protocol), and embedded replica
(local with sync). Turso's rewrite of SQLite in Rust aims at the same SQL
dialect, the same file format and **the same C API**, and ships bindings for
JavaScript/Wasm, Python, Java and Rust.

The finding is a cost, not a feature. Hrana exists because libSQL sells a
*hosted* database and a client that has to reach across a network; the protocol
is the product. inillucent has no server, and the first consumer is a desktop
application opening a file on the same disk. Building a wire protocol to reach
Python would mean shipping and supervising a server process so that two
libraries on one machine could talk. §11 records it as rejected with the reason,
because it is the design somebody will suggest.

Turso's confirmation is worth stating plainly: a from-scratch Rust
reimplementation of an embedded database, in 2026, with every resource, still
reaches four languages through a C-compatible surface.

### 3.6 The Rust FFI rules

The boundary rules are not a matter of taste and the search literature is
unanimous:

- `extern "C"` with `#[no_mangle]`, because without it the calling convention is
  Rust's own and unstable.
- **A panic must never cross the boundary.** Unwinding out of an `extern "C"`
  function is undefined behaviour; the entry point catches with
  `std::panic::catch_unwind` and returns a status. §7.6.
- **Opaque handles.** A foreign caller holding only a pointer needs no
  `#[repr(C)]` and no knowledge of the layout, which is what lets the Rust side
  change shape freely. Every handle in §7 is opaque.

---

## 4. The capability surface

This is the section the design turns on.

### 4.1 The problem, stated exactly

The engine refuses things. That is correct and deliberate — a refusal is what it
ships instead of a wrong answer, and `_agent_output/task-1834-phase5/README.md`
records the one time a wrong answer got as far as being stored (a trigger
written to the schema and never fired) as a defect that was fixed by refusing.

But **a refusal is only useful to an application that can recognise one**, and
today it cannot. Measured in the tree at `b94b8da`:

- `inillucent-exec/src/physical.rs:703` is one helper behind all 37 of that
  crate's refusals:
  ```rust
  fn unsupported<T>(what: &str) -> DbResult<T> {
      Err(misuse(format!("the new engine's physical pass does not handle {what} yet")))
  }
  ```
- `inillucent-sql/src/bind.rs:4063` produces `ParseErrorKind::Unsupported(what)`,
  documented as *"a construct the grammar has but this phase does not
  implement"*, and kept distinct from `ParseErrorKind::Refused(String)`, which is
  *"a statement the schema refuses, in the reference's own wording"*.
- `inillucent-engine/src/lib.rs` then converts **both**, and every syntax error,
  and every `no such table`, with the same expression: `misuse(error.message())`.

So `SELECT * FROM peple` and `SELECT * FROM people LEFT JOIN teams …` come back
as the same code, `SQLITE_MISUSE`, differing only in prose. An application that
wanted to grey out a control, or tell a person "this engine cannot do that yet"
rather than "check your spelling", would have to match on the sentence. Matching
on a sentence is the failure mode this repository has a name for: it works until
somebody improves the wording.

#### 4.2 What the engine cannot do, as of `b94b8da`

> **Read as a dated snapshot, and it has already moved.** This table was taken
> at `b94b8da` and is left as it was written, because §4.4's whole argument is
> that a capability list decays and the instrument is what catches it - so a
> table quietly edited later would be the thing this section warns against.
>
> What actually happened between this design and its implementation is the best
> evidence the design works. task-1838, task-1844 and task-1845 landed outer
> joins, recursive CTEs, derived tables in `FROM`, foreign-key enforcement,
> triggers, `ATTACH`, temporary databases, `STRICT`, and `ALTER TABLE ADD
> COLUMN ... DEFAULT` filling existing rows. **Every one of them turned
> `tests/capability.rs` red with a message naming the row to update**, rather
> than leaving an application refusing to offer something that works.
> `user_functions` and `user_collations` also arrived, and §4.4's postscript
> records how they were missed for a while and what was changed so that they
> cannot be again.
>
> As shipped, the engine answers every row of the table but one. `cancel` is
> the exception, and §4.3 is why it is structural rather than unfinished.


Taken from `_agent_output/task-1834-phase5/README.md` §§13–14, each with a red
qualification suite naming it. This is the driver's initial capability table:

| capability | state | evidence |
|---|---|---|
| foreign key enforcement | **no** | `foreign_keys.rs`, 17 red — the executor cannot fire a `BoundTrigger`, and a foreign key is compiled to one |
| `CREATE TRIGGER` | **refused by name** | §11 of that README; refusing is the fix for a trigger stored and never fired |
| `LEFT` / `RIGHT` / `FULL OUTER JOIN` | **no** | `physical.rs:892`, `joins_match_the_oracle` |
| recursive CTE | **no** | `physical.rs:1079`, `ctes_match_the_oracle` |
| a derived table in `FROM` | **no** | `physical.rs:1047` |
| user-defined scalar / aggregate function | **no** | binder resolves against a static table; a registry no statement can reach is a branch no input can take |
| user-defined collation | **no** | same |
| `ATTACH` / temporary database | **no** | `attach.rs` 13 red, `temp_objects.rs` 11 red — one pool per file |
| `STRICT` enforcement | **no** | `schema_forms.rs` |
| `ALTER TABLE ADD COLUMN … DEFAULT` filling existing rows | **no** | leaves them NULL — `schema_forms.rs` |
| views carried by the SQLite importer | **no** | `import_into` never looks at a view |
| `REINDEX`, `VACUUM INTO`, `CREATE INDEX` on `WITHOUT ROWID` | **no** | `schema_forms.rs` |
| statement cancellation | **no** | §4.3 |
| DDL, DML, `BEGIN`/`COMMIT`/`ROLLBACK`, savepoints | **yes** | Phase 5 §10 |
| checkpoint, crash recovery, reopen by path | **yes** | Phase 5 §§5j–5l |
| bound parameters, `RETURNING`, window functions, inner joins, subqueries as values in most positions | **yes** | Phase 5 §5m: 46 constructs run, 7 refused |

### 4.3 Cancellation, and why it is a capability rather than a bug

`unluminous_db::Stopper` exists because *"the thread running the statement is
inside the engine and cannot look at a flag, so the thing that stops it has to
be reachable from somewhere else"* — PostgreSQL opens a second connection, SQLite
calls `sqlite3_interrupt`.

inillucent has neither, and the reason is structural rather than unfinished.
`inillucent-engine`'s `Statement::step` is documented as materialising: *"`step`
runs the whole statement on its first call and then walks the rows it
produced"*, because the executor is batch-at-a-time and its sinks collect. There
is no row loop in which a flag would be read, so a `cancel()` that set one would
be a function that returns success and does nothing until the statement finishes
of its own accord — which is the exact shape of the defect this engine has
already refused to ship once.

So `INILLUCENT_CAP_CANCEL` is `false`, `inillucent_connection_cancel` returns
`INILLUCENT_STATUS_UNSUPPORTED`, and an application that wants a Stop button
knows before it draws one. When the executor grows a row-at-a-time path — which
Phase 5's own note on `step` says is on the list — the capability flips, one
test changes, and no application has to be rewritten.

### 4.4 The rule that keeps the list honest

**Every row of the capability table is asserted by a test that runs the
construct against a real database and compares the answer to the declaration.**

The test is symmetric, and the second half is the point:

- A capability declared **supported** whose construct fails → the test fails.
  The driver was lying about something it offered.
- A capability declared **unsupported** whose construct now **succeeds** → the
  test fails, saying *"`outer_join` is declared unsupported and it worked; the
  engine has grown this — update the table."*

Without the second half, the table is JDBC's: a set of claims that were true
once, decaying quietly as the engine improves, until an application is refusing
to offer something that has worked for six months. With it, Phase 6 landing
outer joins produces a failing test in the driver that says what to change.

This is the same instrument this repository already uses for its dependency
graph — `docs/invariants/layering.toml` is *"checked, not documented"* — applied
to a different contract.

### 4.4a The hole this rule had, and what closed it

Written as designed, §4.4 exempted a row with **no probe** - a capability the
driver exposed no entry point for - and asked only that its note explain why.
Two rows took that exemption: `user_functions` and `user_collations`, whose
notes said the binder resolves a name against a static table and would never
reach a registry, so *"the call and this row arrive together"*.

The engine then grew `create_scalar_function` and `create_collation`, and the
call did not arrive with them. The rows sat there saying "no" about something
that had worked for weeks - **decaying exactly the way a JDBC list decays, and
for exactly the reason: nothing ran them.** The rule had a hole and the hole was
the exemption.

So the exemption is gone wherever it can be. The driver exposes both calls, and
the rows carry a `Probe::Registers` that registers a doubling function and a
case-insensitive collation and then **calls them from SQL** - because a registry
no statement can reach is precisely the failure those notes described, and a
probe that only checked the registration returned `Ok` would have proved
nothing. What is left with no probe is `cancel` and `readonly_open`, and those
are properties of the driver rather than of the engine.

### 4.5 The two ways a capability is reported

Both, because they answer different questions.

- **Ahead of time**, so an application can decide what to offer:
  `inillucent_capabilities()` returns the whole table; the Rust driver has
  `Capabilities::supports(Capability::OuterJoin)`.
- **At the point of refusal**, because a capability list is coarse and the
  engine's refusals are fine-grained — "a rowid range as an inner join term" is
  not a row anybody would put in a table. A failed statement carries
  `status = Unsupported` and a `feature` string that is the engine's own
  `what`, so an application can say *"this engine cannot do that yet: an outer
  join"* without owning a list of every phrase.

### 4.6 What this costs the engine: one field, no behaviour

The driver must not recover the classification by matching the message, because
that is a second implementation of a fact the engine already has. So the fact is
carried instead of re-derived, additively:

1. `inillucent-base::error::DbError` gains an optional `unsupported: Option<String>`
   inside its already-boxed `ErrorContext`, with `with_unsupported(what)` and
   `unsupported()`. No new error code, no row in `compat/errors.toml` — that
   file is pinned to SQLite 3.53.4's own table and inventing a row in it would
   corrupt the thing it exists to be.
2. `inillucent-exec::physical::unsupported` attaches it. The message is
   character-for-character what it was, so every existing assertion about
   wording still holds.
3. `inillucent-engine`'s three `misuse(error.message())` conversions become a
   helper that attaches it when the `ParseErrorKind` is `Unsupported`.

Total: two accessors, one changed helper, three changed call sites. No message,
no code, and no behaviour changes; the only observable difference is that a
caller who asks `error.unsupported()` gets an answer instead of `None`.

---

## 5. Six decisions, with the reason each was not the other thing

### 5.1 Values are typed, not text

The consumer's PostgreSQL client takes every value as *the text the server
printed*, deliberately, because asking PostgreSQL for binary means writing one
decoder per type OID and then rendering a `numeric` differently from `psql`.

That argument is about a wire protocol, and it does not survive the trip
in-process. The engine hands us an `OwnedDatum` that is already
`Null | Int | Real | Text | Blob`; flattening it to a string would mean this
driver choosing a float's formatting on the application's behalf, and the
application then parsing the string back if it wanted the number. So:

```
Value = Null | Integer(i64) | Real(f64) | Text(String) | Blob(Vec<u8>)
```

which is what `rusqlite` gives the consumer's SQLite arm today, so the adapter
in `unluminous-db` is the same shape it already has. The `Null` variant is
distinct from empty text, per §2.

### 5.2 One database, one connection, and it is said out loud

`inillucent-engine::connect`'s own comment: *"this engine is single threaded and
one file is one pool: two connections that each held a pool over one file would
be two page caches over one set of bytes."*

The driver does not paper over that with a pool, and does not let a caller find
it out by corrupting something. A `Database` is `!Sync`; a second `open` of the
same path is a caller's decision the driver cannot police across processes, but
within one process the C ABI's `inillucent_open` refuses a path it already has
open, by name, with `INILLUCENT_STATUS_INVALID_STATE`. A binding that wants to
use one database from several threads owns the serialisation, and §9.3 says so
in the words a binding author needs.

### 5.3 `more` is counted, not guessed

The consumer asks its servers for `limit + 1` rows and cuts back, so that
`1–200 of 200+` never claims a count nobody took.

We cannot do that here without rewriting the caller's SQL, and rewriting a
caller's statement is the failure four instruments in this project have already
had. But we do not need to: the engine materialises, so by the time the driver
sees a result it has **every** row and the true count is a `Vec::len()`. The
driver truncates to `limit`, sets `more = produced > limit`, and reports
`total = produced` exactly.

The cost is real and is written down rather than hidden: a query over a large
table costs what the whole result costs, not what one page costs, because that
is what the engine underneath does. `Rows::total` is therefore an exact number
and `more` is an exact fact, which is *stronger* than what the PostgreSQL arm
can offer — and the driver documents the memory, because a consumer choosing a
`LIMIT` in its own SQL is the way to avoid it.

### 5.4 A batch is one transaction, and the check happens before the commit

The consumer's rule, kept verbatim: every statement of a row-editor write must
change **exactly one** row, and *"the check is handed in rather than run
afterwards, because SQLite's transaction commits inside that call: a
postcondition tested after the commit is a report about something that has
already happened rather than a guard against it."*

So `Connection::transaction(statements, check)` takes the predicate as an
argument, runs `BEGIN`, runs each statement, calls `check(&affected)`, and
issues `COMMIT` only if it returns `Ok` — `ROLLBACK` on any error and on a
failed check. In C, where a callback is awkward, the same guarantee is given by
an explicit handle: `inillucent_txn_begin` / `_execute` / `_affected` /
`_commit` / `_rollback`, with the caller doing its own checking between the last
`_execute` and `_commit`, and a dropped handle rolling back.

### 5.5 The diagnostic detail does not cross the boundary by default

`inillucent-base`'s error module is explicit: `message` is *"safe to hand to a
caller and never contains a file-system path, a bound value, or page bytes"*,
while `detail` is *"for diagnostics that stay inside the process unless an
explicit diagnostic callback asks for them"*, and there is a test named
`display_hides_internal_detail`.

The driver keeps that seam. `inillucent_error_message` returns the safe message;
`inillucent_error_detail` exists, returns `NULL` unless the database was opened
with `INILLUCENT_OPEN_DIAGNOSTICS`, and is documented as returning text that may
contain paths and values and must not be shown to a user or logged to a shared
sink.

### 5.6 The driver refuses to be a compatibility layer

`crates/inillucent-capi` reproduces the SQLite C ABI — the same symbols, the
same structs, the same ownership — over the *old* engine, in 5,330 lines. It is
on Phase 6's deletion list.

This driver does not replace it and is not shaped like it. Reproducing
`sqlite3.h` would mean either implementing 250 entry points over an engine that
cannot answer a quarter of them, or implementing a subset and letting a caller
who links `sqlite3.h` discover the difference at run time — the "drop-in
replacement that is not one" that makes a caller's existing code fail in a place
that has nothing to do with the change. The driver has its own small surface and
its own name, so nothing is a drop-in for anything and a caller who wants
inillucent asks for it.

---

## 6. The Rust driver — `drivers/inillucent-driver`

### 6.1 It is the driver, not a binding

The Rust surface holds every decision in §§4–5 and depends on `inillucent-engine`
and nothing else in the workspace. The C ABI in §7 is a marshalling layer over
it that decides nothing.

Concretely, that is why: the first consumer is Rust, and making it call
`extern "C"` into a Rust engine would add a pointer round trip and a
`catch_unwind` per call for no gain, lose the type system across the seam, and
put the errors a Rust caller sees through a C error code and back. DuckDB routes
its own Rust binding through the C ABI because its core is C++; ours is not.

### 6.2 The stable surface

```rust
// —— what the driver is ————————————————————————————————————————
pub fn version() -> &'static str;               // "inillucent-driver 0.1.0 (engine 0.1.0)"
pub fn capabilities() -> &'static Capabilities;

// —— a database file ———————————————————————————————————————————
pub struct Database { /* opaque */ }
impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Database>;
    pub fn open_with(path: impl AsRef<Path>, options: OpenOptions) -> Result<Database>;
    pub fn connect(&self) -> Connection<'_>;
    pub fn path(&self) -> &Path;
    pub fn checkpoint(&self) -> Result<()>;
    pub fn integrity_check(&self) -> Result<()>;
    pub fn backup_to(&self, path: impl AsRef<Path>) -> Result<()>;
    pub fn close(self) -> Result<()>;           // checkpoints; Drop does too, silently
}

pub struct OpenOptions {
    pub create: bool,        // default true
    pub read_only: bool,     // default false — see §6.4
    pub cache_frames: usize, // default 4096 (128 MiB at the engine's 32 KiB page)
    pub diagnostics: bool,   // default false — see §5.5
}

// —— a connection ——————————————————————————————————————————————
pub struct Connection<'d> { /* opaque */ }
impl Connection<'_> {
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<Rows>;
    pub fn execute_batch(&self, sql: &str) -> Result<()>;
    pub fn query(&self, sql: &str, params: &[Value], limit: usize) -> Result<Rows>;
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>>;
    pub fn explain(&self, sql: &str) -> Result<Vec<String>>;
    pub fn transaction<F>(&self, work: &[(String, Vec<Value>)], check: F) -> Result<Vec<u64>>
        where F: Fn(&[u64]) -> Result<()>;
    pub fn last_insert_rowid(&self) -> i64;
    pub fn total_changes(&self) -> i64;
    pub fn in_transaction(&self) -> bool;
    pub fn schema_cookie(&self) -> u64;
    // —— introspection: §6.5 ——
    pub fn schemas(&self) -> Result<Vec<String>>;
    pub fn items(&self) -> Result<Vec<Item>>;
    pub fn table(&self, name: &str) -> Result<Table>;
    pub fn ddl(&self, name: &str) -> Result<String>;
}

// —— a compiled statement ——————————————————————————————————————
pub struct Statement<'c> { /* opaque */ }
impl Statement<'_> {
    pub fn execute(&mut self, params: &[Value]) -> Result<Rows>;
    pub fn parameter_count(&self) -> u32;
    pub fn sql(&self) -> &str;
}

// —— what comes back ——————————————————————————————————————————
pub struct Rows {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    pub affected: Option<u64>,
    pub total: usize,          // every row the statement produced, exactly (§5.3)
    pub more: bool,            // total > rows.len()
    pub elapsed: Duration,
    pub tag: String,           // "SELECT 27", "UPDATE 1" — the driver's own words
}
pub struct Column { pub name: String, pub declared_type: String }
pub enum Value { Null, Integer(i64), Real(f64), Text(String), Blob(Vec<u8>) }
```

### 6.3 Errors

```rust
pub struct Error {
    pub status: Status,
    pub message: String,          // safe; never a path or a bound value
    pub detail: Option<String>,   // only when opened with diagnostics (§5.5)
    pub feature: Option<String>,  // set iff status == Unsupported (§4.5)
    pub offset: Option<u32>,      // byte offset into the statement
    pub engine_code: i32,         // the extended result code, for diagnosis
}

pub enum Status {
    Unsupported,   // this engine cannot do that yet — §4
    Syntax,        // the statement is not valid SQL
    NotFound,      // no such table / column / index
    Constraint,    // a constraint refused the write
    ReadOnly, Busy, Interrupted, Corrupt, Io, Full, TooBig,
    InvalidState,  // a driver contract the caller broke
    Internal,      // a defect — the only status that means "report this"
}
```

`Status` is ADBC's list (§3.3) with the members this engine cannot produce
removed — no `Unauthenticated`, no `Unauthorized`, no `Timeout` — because a
status nothing can return is a branch no input can take.

Mapping, and the order matters because the first match wins:

| condition | status |
|---|---|
| `DbError::unsupported()` is `Some` | `Unsupported`, `feature` = its value |
| extended code's primary is `Constraint` | `Constraint` |
| `ReadOnly` / `Busy` / `Interrupt` / `Corrupt` / `IoErr` / `Full` / `TooBig` | the same name |
| `Misuse`, message begins `no such ` | `NotFound` |
| `Misuse`, anything else | `Syntax` |
| `Internal` | `Internal` |

The `no such ` prefix is the one string comparison in the driver and it is here
because it is SQLite's own stable wording, reproduced deliberately by
`no_such_table` in `inillucent-sql/src/bind.rs` with a comment saying so. It is
recorded as the design's one soft spot: if `NotFound` ever has to be exact, the
`Refused` kind carries it and §4.6's mechanism generalises in one line.

### 6.4 Read-only

The engine has no read-only open, so a driver claiming one would be enforcing it
itself. It does — by classifying the compiled statement, not by scanning the
text: `Connection` asks the engine to compile, and refuses anything that is not
a `SELECT` or a read-only pragma with `Status::ReadOnly`. That is a real
guarantee because the classification is the binder's, not a regex's. It is
weaker than SQLite's `SQLITE_OPEN_READONLY`, which is the file handle, and the
capability table says so: `readonly_open` is `partial`, with the note *"enforced
by the driver above the engine, not by the file"*.

### 6.5 Introspection

Every one of these is SQL the engine already answers, so there is one schema
reader in the tree rather than two that can disagree:

| driver call | statement |
|---|---|
| `schemas()` | constant `["main"]` |
| `items()` | `SELECT type, name FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name` |
| `table(name)` | `PRAGMA table_info(<quoted>)`, then `PRAGMA index_list(<quoted>)` |
| `ddl(name)` | `SELECT sql FROM sqlite_schema WHERE name = ?1` |

`Table` carries `columns`, `key` (in key order, from `table_info`'s `pk`), and
`without_rowid`. It does **not** invent a `rowid` alias the way the consumer's
SQLite arm does: whether a hidden key is addressable is the consumer's rule,
and the driver's job is to report `without_rowid` and the declared columns
faithfully so the consumer can apply it.

---

## 7. The C ABI — `drivers/inillucent-driver-capi`

The contract every non-Rust binding is written against. A binding author needs
§§7–10 and nothing else.

### 7.1 Shape

`inillucent_driver.h`, one header, no dependencies beyond `stdint.h` and
`stddef.h`. Built as `cdylib` and `staticlib`. Everything is an opaque pointer;
nothing crosses the boundary by value except integers, doubles, and pointers to
bytes the header says who owns.

```c
typedef struct inillucent_db      inillucent_db;      /* an open file        */
typedef struct inillucent_conn    inillucent_conn;    /* a connection        */
typedef struct inillucent_stmt    inillucent_stmt;    /* a compiled statement*/
typedef struct inillucent_rows    inillucent_rows;    /* a result            */
typedef struct inillucent_txn     inillucent_txn;     /* an open transaction */
typedef struct inillucent_error   inillucent_error;   /* a failure           */
```

### 7.2 Status codes

```c
#define INILLUCENT_OK             0
#define INILLUCENT_UNSUPPORTED    1   /* this engine cannot do that yet   */
#define INILLUCENT_SYNTAX         2
#define INILLUCENT_NOT_FOUND      3
#define INILLUCENT_CONSTRAINT     4
#define INILLUCENT_READONLY       5
#define INILLUCENT_BUSY           6
#define INILLUCENT_INTERRUPTED    7
#define INILLUCENT_CORRUPT        8
#define INILLUCENT_IO             9
#define INILLUCENT_FULL          10
#define INILLUCENT_TOO_BIG       11
#define INILLUCENT_INVALID_STATE 12
#define INILLUCENT_INTERNAL      13
```

Numbers are frozen at 1.0 and only appended to.

### 7.3 The entry points

Thirty-one. Every one returns `int32_t` (a status) except the accessors, which
cannot fail and say so.

```c
/* —— the library ———————————————————————————————————————————— */
uint32_t    inillucent_abi_version(void);
const char *inillucent_version(void);
size_t      inillucent_capability_count(void);
int32_t     inillucent_capability(size_t nth, const char **name,
                                  int32_t *state, const char **note);
int32_t     inillucent_supports(const char *name);   /* 1 yes, 0 no, -1 partial, -2 unknown */

/* —— a database ——————————————————————————————————————————— */
int32_t inillucent_open(const char *path, uint32_t flags,
                        inillucent_db **out, inillucent_error **error);
int32_t inillucent_close(inillucent_db *db, inillucent_error **error);
int32_t inillucent_checkpoint(inillucent_db *db, inillucent_error **error);
int32_t inillucent_integrity_check(inillucent_db *db, inillucent_error **error);
int32_t inillucent_backup_to(inillucent_db *db, const char *path, inillucent_error **error);

/* —— a connection ————————————————————————————————————————— */
int32_t inillucent_connect(inillucent_db *db, inillucent_conn **out, inillucent_error **error);
void    inillucent_conn_free(inillucent_conn *conn);
int32_t inillucent_execute(inillucent_conn *conn, const char *sql,
                           inillucent_rows **out, inillucent_error **error);
int32_t inillucent_execute_batch(inillucent_conn *conn, const char *sql, inillucent_error **error);
int64_t inillucent_last_insert_rowid(inillucent_conn *conn);
int64_t inillucent_total_changes(inillucent_conn *conn);
int32_t inillucent_in_transaction(inillucent_conn *conn);
int32_t inillucent_cancel(inillucent_conn *conn, inillucent_error **error); /* always UNSUPPORTED */

/* —— a statement ——————————————————————————————————————————— */
int32_t inillucent_prepare(inillucent_conn *conn, const char *sql,
                           inillucent_stmt **out, inillucent_error **error);
void    inillucent_stmt_free(inillucent_stmt *stmt);
int32_t inillucent_bind_null(inillucent_stmt *stmt, uint32_t index);
int32_t inillucent_bind_int(inillucent_stmt *stmt, uint32_t index, int64_t value);
int32_t inillucent_bind_real(inillucent_stmt *stmt, uint32_t index, double value);
int32_t inillucent_bind_text(inillucent_stmt *stmt, uint32_t index, const char *value, size_t len);
int32_t inillucent_bind_blob(inillucent_stmt *stmt, uint32_t index, const uint8_t *value, size_t len);
int32_t inillucent_stmt_execute(inillucent_stmt *stmt, uint64_t limit,
                                inillucent_rows **out, inillucent_error **error);

/* —— a result ——————————————————————————————————————————————— */
void        inillucent_rows_free(inillucent_rows *rows);
size_t      inillucent_rows_column_count(const inillucent_rows *rows);
const char *inillucent_rows_column_name(const inillucent_rows *rows, size_t nth);
const char *inillucent_rows_column_type(const inillucent_rows *rows, size_t nth);
size_t      inillucent_rows_count(const inillucent_rows *rows);
size_t      inillucent_rows_total(const inillucent_rows *rows);
int32_t     inillucent_rows_more(const inillucent_rows *rows);
int64_t     inillucent_rows_affected(const inillucent_rows *rows); /* -1 = not a write */
uint64_t    inillucent_rows_elapsed_us(const inillucent_rows *rows);
int32_t     inillucent_value_type(const inillucent_rows *rows, size_t row, size_t column);
int64_t     inillucent_value_int(const inillucent_rows *rows, size_t row, size_t column);
double      inillucent_value_real(const inillucent_rows *rows, size_t row, size_t column);
const uint8_t *inillucent_value_bytes(const inillucent_rows *rows, size_t row, size_t column,
                                      size_t *len);

/* —— a transaction ————————————————————————————————————————— */
int32_t inillucent_txn_begin(inillucent_conn *conn, inillucent_txn **out, inillucent_error **error);
int32_t inillucent_txn_execute(inillucent_txn *txn, const char *sql,
                               uint64_t *affected, inillucent_error **error);
int32_t inillucent_txn_commit(inillucent_txn *txn, inillucent_error **error);
void    inillucent_txn_rollback(inillucent_txn *txn);

/* —— a failure ——————————————————————————————————————————————— */
int32_t     inillucent_error_status(const inillucent_error *error);
const char *inillucent_error_message(const inillucent_error *error);
const char *inillucent_error_feature(const inillucent_error *error); /* NULL unless UNSUPPORTED */
const char *inillucent_error_detail(const inillucent_error *error);  /* NULL unless diagnostics */
int32_t     inillucent_error_offset(const inillucent_error *error);  /* -1 when there is none */
void        inillucent_error_free(inillucent_error *error);
```

### 7.4 Ownership — three rules and nothing else

1. **A handle a `_free` names is the caller's to free**, exactly once, and
   freeing `NULL` is a no-op so a binding's finaliser need not check.
   `inillucent_db`, `inillucent_conn`, `inillucent_stmt`, `inillucent_rows`,
   `inillucent_txn`, `inillucent_error`.
2. **Every `const char *` and `const uint8_t *` this library returns points
   inside the handle it was asked of**, is valid until that handle is freed, and
   is never freed by the caller. It does not move: `inillucent_rows` is
   materialised, so a pointer into row 0 stays valid while row 900,000 is read.
   Text is UTF-8 and NUL-terminated; a blob is not, which is why
   `inillucent_value_bytes` takes a length out-parameter.
3. **Every pointer the caller hands in is copied before the call returns.**
   `inillucent_bind_text` does not borrow. A binding may free its buffer on the
   next line.

Lifetimes are nested and checked: freeing a `inillucent_db` with connections
still open returns `INILLUCENT_INVALID_STATE` rather than leaving dangling
handles, and §9.2 tells a binding author to tie the child's lifetime to the
parent's in the host language's own idiom.

### 7.5 Versioning

`inillucent_abi_version()` returns `major * 1000000 + minor * 1000 + patch`,
which is ADBC's encoding. A binding checks the major at load and refuses a
mismatch by name — the whole point being that it refuses rather than calling a
function whose signature has moved.

Every entry point carries a stability tag in the header and in
`drivers/abi.toml`:

- **stable** — frozen; the signature will not change and the symbol will not be
  removed. Every function in §7.3 is stable at 1.0 except those marked below.
- **provisional** — may change in a minor version.
  `inillucent_cancel` is provisional, because it exists to return
  `UNSUPPORTED` today and will grow a real meaning (§4.3).

`drivers/abi.toml` is checked the way `layering.toml` is: a test parses the
header, parses the manifest, and fails on a symbol in one and not the other, or
a stability tag that has been weakened.

### 7.6 The boundary

Every entry point is `extern "C"`, `#[no_mangle]`, and wraps its body in
`std::panic::catch_unwind`, returning `INILLUCENT_INTERNAL` with a message
naming the entry point if it catches. A panic reaching a C caller is undefined
behaviour, and the driver is `#![deny(clippy::unwrap_used)]` and
`#![deny(clippy::panic)]` so the catch is a backstop and not a strategy.

A NULL handle where the header does not permit one returns
`INILLUCENT_INVALID_STATE`, never a crash.

---

## 8. The conformance suite

**A specification nobody can run is a specification every implementation reads
differently.** So the driver's behaviour is defined by a data file rather than
by prose, and the Rust driver, the C ABI and every future binding run the same
file.

`drivers/conformance/suite.json`:

```json
{
  "version": 1,
  "cases": [
    { "name": "insert_then_select",
      "setup":  ["CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)"],
      "steps":  [
        { "sql": "INSERT INTO t VALUES (1, 'one')", "affected": 1 },
        { "sql": "SELECT a, b FROM t",
          "columns": ["a", "b"],
          "rows": [[{"int": 1}, {"text": "one"}]] }
      ] },
    { "name": "outer_join_is_refused_by_name",
      "setup":  ["CREATE TABLE l (a INTEGER)", "CREATE TABLE r (a INTEGER)"],
      "steps":  [
        { "sql": "SELECT * FROM l LEFT JOIN r ON l.a = r.a",
          "status": "unsupported",
          "feature_contains": "outer join" }
      ] }
  ]
}
```

A runner is forty lines in any language: open a scratch database, run `setup`,
run each step, compare. Cases cover the round trip of all five value types
including a blob with an embedded NUL and text with a non-ASCII character,
parameter binding, `affected` counts, `total`/`more` at a limit, transaction
commit and rollback, the exactly-one-row check of §5.4, every status in §7.2
that this engine can produce, and one case per **unsupported** capability
asserting that it fails with `unsupported` and names itself.

That last group is §4.4's instrument in portable form: a binding in any language
finds out the same way the Rust one does when a capability the table calls
missing starts working.

---

## 9. Writing a binding in another language

### 9.1 The shape every binding has

1. Load the shared library (`inillucent_driver.dll` / `.so` / `.dylib`) and
   check `inillucent_abi_version()`'s major against the one you were written
   for. Refuse by name on a mismatch.
2. Wrap each opaque pointer in the host language's own resource type, with a
   finaliser calling the matching `_free`, and make the finaliser safe to run
   twice.
3. After **every** call that takes an `inillucent_error **`, check the status
   first. On non-zero, build the host language's exception from
   `inillucent_error_status`, `_message`, `_feature` and `_offset`, then call
   `inillucent_error_free` — and do it in the host's `finally`, because an
   exception constructed from the error must not leak it.
4. Map `INILLUCENT_UNSUPPORTED` to **its own exception type**, not to the
   general one. That is the whole of §4 arriving in the host language, and a
   binding that folds it into a generic `DatabaseError` has thrown the design
   away.
5. Copy every `const char *` into the host's own string on the way out. It
   points into a handle the caller may free.
6. Run `drivers/conformance/suite.json`. A binding that has not run it is a
   binding that has not been tested.

### 9.2 Lifetimes in a garbage-collected language

A connection is only valid while its database is, and non-deterministic
finalisation will otherwise free them in the wrong order. Every binding holds a
strong reference from child to parent — the connection object keeps the database
object alive, the statement keeps the connection, the rows keep nothing because
they are materialised and self-contained. `inillucent_rows` is deliberately
independent for exactly this reason: it is the handle most likely to outlive its
statement in a language with a `for row in query(...)` idiom.

### 9.3 Threads

The engine is single threaded and one file is one pool. A binding must either
confine a `inillucent_db` and everything under it to one thread, or serialise
every call on it with a mutex the binding owns. There is no internal lock, and
the driver does not pretend there is: `inillucent_db` is not thread-safe and
this sentence is the contract.

Two databases on two files are independent and may be used from two threads.

### 9.4 Known-good starting points

- **Python** — `ctypes` from the standard library. `drivers/bindings/python/`
  is the reference implementation and is deliberately dependency-free so it can
  be read as documentation.
- **Node** — `koffi`, or N-API if a compiled addon is acceptable.
- **Go** — `cgo`, or `purego` to avoid it.
- **Java** — the Foreign Function & Memory API on 22+; JNI below that.
- **C#** — `DllImport` with `SafeHandle` subclasses, which give §9.2 for free.

---

## 10. Layout

```
drivers/
  README.md                         the binding author's front door
  abi.toml                          every C symbol, its stability, its since-version
  inillucent-driver/                the Rust driver (§6)
    src/{lib,value,rows,error,capability,introspect,transaction}.rs
    tests/{driver,capability,conformance}.rs
  inillucent-driver-capi/           the C ABI (§7)
    include/inillucent_driver.h
    src/{lib,handle,rows,error}.rs
    tests/abi.rs                    header and manifest agree
  conformance/
    suite.json                      §8
    README.md
  bindings/
    python/inillucent.py            the reference binding (§9.4)
    python/run_conformance.py
```

Both crates are workspace members and both are declared in
`docs/invariants/layering.toml`: `inillucent-driver` at layer 9 depending on
`inillucent-base` and `inillucent-engine`; `inillucent-driver-capi` at layer 10
depending on `inillucent-driver`.

The layering check reads only `<root>/crates`, so a member outside it is
invisible to the contract. That is a hole today rather than a hole this ticket
makes — nothing outside `crates/` has ever been checked — so `read_workspace`
is changed to read the **workspace's own `members` list** from the root
manifest. Every existing member is found exactly as before, and any member added
anywhere in future is checked from the moment it exists.

---

## 11. Alternatives, and why each is not this

| alternative | why not |
|---|---|
| **A PostgreSQL wire-protocol server** | Every language already has a client, and the consumer already has a hand-written one. But it means a server process, a port, authentication, and a network hop between two libraries on one disk — the whole cost of the thing this engine exists not to be. It also promises PostgreSQL semantics over a SQLite-dialect engine, which is a wrong answer waiting for a `RETURNING` clause. |
| **ADBC** | The right standard for analytics and worth doing later (§3.3). Its result sets are Arrow streams, which is a large dependency and a poor fit for a grid, and it would put an Arrow C data interface between this engine and its first consumer. Additive over `inillucent-driver` whenever somebody wants it. |
| **Reproduce SQLite's C ABI** | §5.6. Already tried in `inillucent-capi`, 5,330 lines, on the deletion list, and it promises a compatibility this engine does not have. |
| **A per-language reimplementation of the engine** | Not a driver. |
| **A subprocess speaking JSON over stdio** | Cheap to write, and it makes every query a serialisation round trip through a pipe, which is slower than the query. Reasonable for a scripting convenience, not for a driver. |
| **`unluminous-db` depends on `inillucent-engine` directly** | It would work today and it is what task-1837 exists to prevent: a cross-repo dependency on internal crates whose deletion is scheduled by Phase 6, so `unluminous` would break on a change inillucent is entitled to make. One stable crate is the seam. |

---

## 12. Definition of done, and what shipped

1. `drivers/inillucent-driver` builds, and its tests pass. **Done** — 18 unit
   tests, plus the two below.
2. `drivers/inillucent-driver-capi` builds as `cdylib` and `staticlib`, and the
   header and `abi.toml` agree. **Done** — 53 symbols, 5 checks in
   `tests/abi.rs`, including that the header's numeric constants *are* the
   driver's own enum values. The numbers are the ABI, and a variant renumbered
   on the Rust side without the header changing would make every binding in
   every language silently misread every error with both sides still compiling.
3. The capability table is asserted **in both directions**. **Done** —
   `tests/capability.rs`, 24 rows, and §4.4a records the hole this rule had and
   what closed it.
4. `drivers/conformance/suite.json` passes from the Rust runner. **Done** — 15
   cases, 51 steps.
5. It passes from a second language. **Done** —
   `python drivers/bindings/python/run_conformance.py`, standard library only,
   through the C ABI: 15 cases, 51 steps, 0 failures, the same numbers the Rust
   runner reports.
6. The layering contract covers both new crates. **Done** — `inillucent-driver`
   at layer 9 over `inillucent-engine` alone, `inillucent-driver-capi` at layer
   10 over the driver alone.
7. The qualification suites are no more red than they were. **Done, and
   measured rather than argued** — `ordering.rs`'s one failure was attributed by
   stashing every engine change and re-running: the same 7 of 69 statements
   diverge without them, so it is not this ticket's.

### What the work changed in the engine, and why each was necessary

Three changes, all additive, none altering a result code or a row:

- **`DbError::unsupported`** (`inillucent-base`), set by the single
  `unsupported` helper in `inillucent-exec`'s physical pass and by the binder's
  `ParseErrorKind::Unsupported`. Without it the driver would have had to
  recognise a capability gap by matching the wording of a sentence, which works
  until somebody improves the wording. §4.6.
- **`error::refusal`**, and ~60 of `inillucent-engine`'s refusals moved onto it.
  `misuse()` attaches what it is given as *detail* — the field
  `inillucent-base` documents as never leaving the process — so `message()` for
  every SQL refusal answered with its primary code's generic text, "bad
  parameter or other API misuse". A person typing `SELECT a FROM peple` got
  that rather than `no such table: peple`. Two independent workarounds already
  existed in the tree (`inillucent-cli::shell::reason` and `readgate::why`,
  each doing `detail().unwrap_or(message())`), which is what a defect looks
  like when it has been met twice and fixed neither time. The sentence is now
  the message *and* the detail, so every existing reader of `detail()` is
  untouched. The three sites whose text names a file path stay on `misuse`.
- **`RETURNING` reports its column names.** `INSERT ... RETURNING a, b`
  produced its rows and an empty name list — two columns of values with no
  headings — which the conformance suite caught, because a result with rows and
  no columns is a shape nothing else in the engine produces. The names are the
  binder's own, so `SELECT a` and `RETURNING a` cannot disagree about what the
  column is called.

### What is deliberately not here

- **An ADBC driver.** §11 keeps it as an additive option over
  `inillucent-driver` for whenever somebody wants Arrow.
- **Function registration over the C ABI.** The Rust driver registers scalar
  functions and collations; doing it from C means marshalling a callback in
  both directions, and no binding has asked yet. The capability table describes
  the *engine*, so those rows say `yes` and the header is where a C caller
  learns there is no entry point.
- **`unluminous-db`'s `Engine::Inillucent` arm.** That is task-1814, which this
  ticket exists to unblock. §2 is the contract it will bind to.
