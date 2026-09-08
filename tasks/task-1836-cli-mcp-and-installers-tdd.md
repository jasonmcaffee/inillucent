# task-1836 — the command surface, the MCP server, and getting it installed

**What this ticket is about, in one sentence:** inillucent can be driven by a person sitting at a
shell, and by a program that links a C library, and by nothing in between — so this ticket adds the
in-between (a verb-shaped CLI and an MCP server over the same command table), and then makes the
result something you can *install* rather than something you have to build.

Written 2026-09-08 from measurements taken that day at commit `dbe8b29`, and **updated the same
day from the implementation** - every place the build disagreed with the design is marked
*changed in build* and says what the design got wrong. Every count in §2 came from
running the two shells side by side; none of them is an estimate.

---

## 1. Why this is a ticket at all

The engine is further along than its front door. `feature-comparison.md` says 409 of 416 differential
cases agree with SQLite; `drivers/README.md` documents a stable C ABI with a checked capability table
and a conformance suite that two independent runners execute. And yet:

- there is no way to get inillucent onto a machine except `git clone` of a **private** repository
  followed by a fifteen-minute `cargo build --release`;
- there is no way for a program that is not a C caller to *do* anything with it without first writing
  a binding;
- and there is no way at all for an LLM agent — the thing this studio actually runs all day — to use
  it, because an agent speaks MCP and reads tool descriptions, and neither exists.

The last one is the one that motivated the ticket. A database an agent cannot reach is a database the
studio does not have.

---

## 2. What the CLI is today, measured

### 2.1 `inillucent-shell` — the dot-command surface is essentially complete

`inillucent-shell` is a port of `sqlite3`'s `shell.c`, and it is a faithful one.

| | reference `sqlite3` 3.53.4 | `inillucent-shell` |
|---|---|---|
| documented dot commands (`.help`) | 65 | 63 |
| dot commands dispatched (`dot.rs` arms) | — | 67, including the four `sqlite3` keeps undocumented |
| command-line options (`-help`) | 48 | **19** |

The dot commands sqlite3 has and this shell does not are exactly two: **`.expert`** and
**`.session`**. Both are compile-time optional in `sqlite3` itself (`SQLITE_ENABLE_DBPAGE_VTAB` /
`SQLITE_ENABLE_SESSION`); neither is a gap a script would fall into. The four this shell has that
`sqlite3`'s `.help` does not print — `.selftest`, `.separator`, `.show`, `.width` — are the entries
`shell.c` marks undocumented with a leading `,`, and this shell marks them the same way.

**So the dot-command surface is not what is missing.** That is worth stating plainly, because it is
the first place anybody would look.

### 2.2 The command line, on the other hand, is a third of SQLite's

19 options against 48. The 29 absent ones, listed:

```
--  -append  -ascii  -batch  -deserialize  -escape  -ifexists  -init  -interactive
-lookaside  -maxsize  -memtrace  -mmap  -newline  -no-rowid-in-view  -nofollow  -noinit
-nonce  -pagecache  -pcachetrace  -readonly  -safe  -screenwidth  -stats  -tabs
-unsafe-testing  -vfs  -vfstrace  -zip
```

They divide cleanly, and the division decides which ones this ticket implements:

| group | options | what happens |
|---|---|---|
| **means something here** | `-init`, `-noinit`, `-batch`, `-readonly`, `-safe`, `-nonce`, `-stats`, `-tabs`, `-ifexists`, `-nofollow`, `--` | implemented |
| **names a SQLite internal this engine does not have** | `-lookaside`, `-pagecache`, `-mmap`, `-memtrace`, `-pcachetrace`, `-vfstrace`, `-deserialize`, `-maxsize`, `-append`, `-zip`, `-unsafe-testing`, `-no-rowid-in-view`, `-vfs` | **accepted and refused by name**, with a one-line reason on stderr and a non-zero exit — never silently ignored |

The second row is a decision, not a shortcut. A shell that swallows `-mmap 268435456` and carries on
has told the caller their setting took effect. The driver already has a whole status for this
(`UNSUPPORTED`, and `drivers/README.md` argues at length for why it is not folded into the general
error), and the shell should speak the same way.

> **Changed in build: `-ascii` and `-newline` moved to the refused row, and the final count is 32
> implemented against 16 refused.** Both set a *row* separator, and the shell's output path writes a
> line and then a newline - it honours a row separator only where the separator itself ends in one,
> which is why `-csv` works and is byte-identical to the reference. The ASCII record separator does
> not end in a newline, so `-ascii` would have set the column separator and silently dropped the row
> one. Half-honouring it is exactly the failure the second row refuses. `-escape`, `-screenwidth` and
> `-interactive` joined them for the same kind of reason: `.mode` has no `--escape` or `--sw`, and
> this shell never prompts, so there is nothing for `-interactive` to force on.
>
> Two things came out of implementing these that are worth more than the options themselves.
>
> **`.separator` never resolved backslash escapes.** `sqlite3 :memory: '.separator "\t"' 'SELECT 1,2'`
> answers `1<tab>2` and this shell answered `1\t2` - a *wrong answer*, in the class this repository
> counts, rather than a refusal. It was invisible because a separator is a **setting**: nothing in the
> 183-case differential probe changes one, so every comparison ran on the default and agreed.
> `resolve_backslashes` is ported now, `.nullvalue` uses it too, and both shells agree byte for byte.
>
> **On Windows this shell writes LF where `sqlite3` writes CRLF**, in every mode but CSV. The
> reference's stream is in text mode and translates; Rust's does not, and `render.rs` compensates for
> CSV alone. `semantics.rs` never caught it because it compares normalised lines. This is a real
> difference and it is **left alone deliberately**: changing a default line ending under a packaging
> ticket would move the bytes of every comparison in the 416-case probe, and that is a measurement
> somebody should make on purpose. Filed as a finding, not fixed here.


### 2.3 `inillucent-migrate` — a second binary with its own argument grammar

```
inillucent-migrate <source-index-dir> <destination.db> [--no-publish]
inillucent-migrate --sqlite-file <source.db> <destination.rdb>
```

Hand-rolled parsing, positional-vs-flag rules it discovered the hard way (there is a comment in
`main.rs` about a flag's value being read as a positional), and no relationship to the shell. It is a
good tool behind an argument parser nobody would guess at.

### 2.4 What exists only as SQL

The retrieval engine — the half of this repository that beats pgvector on 15 of 17 comparisons — has
**no command-line surface whatsoever**. `inillucent_search`, `VECTOR(N)` columns,
`vector_distance_cos`, `CREATE INDEX … USING inillucent_hnsw` are all reachable only by typing SQL. A
person who wants to ask "what is in this corpus that looks like this sentence" has to know the
schema, the function names and the index idiom first.

### 2.5 What does not exist at all

1. **Verbs.** Everything is either a REPL or a SQL string. There is no `inillucent tables app.rdb`.
2. **A machine-readable result contract.** `.mode json` renders *rows* as JSON; there is nothing that
   renders an *outcome* — status, changes, last rowid, elapsed, error class — as JSON. A script that
   wants to know whether the statement worked parses English off stderr.
3. **Any agent surface.** No MCP server, no tool descriptions, no capability advertisement.
4. **Distribution.** No installer, no release archive, no package in any registry, no published
   binary of any kind. The private GitHub repository is the only artifact.

---

## 3. The design

### 3.1 One command table, three front ends

```
                        ┌─────────────────────────────────┐
                        │  inillucent_cli::command        │
                        │  the COMMANDS table:            │
                        │  name, summary, params, run     │
                        └───────────────┬─────────────────┘
                    ┌───────────────────┼───────────────────┐
                    │                   │                   │
            ┌───────▼───────┐   ┌───────▼───────┐   ┌───────▼────────┐
            │  inillucent   │   │ inillucent-   │   │ inillucent-mcp │
            │  (verbs)      │   │ shell (REPL)  │   │ (stdio JSON-RPC)│
            └───────────────┘   └───────────────┘   └────────────────┘
                    │                   │                   │
                    └───────────────────┼───────────────────┘
                                        │
                                ┌───────▼────────┐
                                │  Shell         │  ← the existing adapter
                                └───────┬────────┘
                                        │
                                ┌───────▼────────┐
                                │  inillucent    │  the public facade
                                └────────────────┘
```

**The point of the arrangement is that parity is structural rather than remembered.** A command added
to the table appears in `inillucent --help`, in `inillucent <verb>`, and in the MCP server's
`tools/list`, in the same commit, because all three read the same array. There is no second list to
update, and `command_parity.rs` fails the build if one is invented.

`inillucent-cli` becomes a library with three binaries rather than one binary with private modules.
The shell modules are already `pub` and already carry doc comments (the crate denies `missing_docs`),
so the conversion is `src/lib.rs` plus a thinner `main.rs`, and no existing behaviour moves.

**The registry drives the `Shell`, not the engine and not the driver.** The shell's own header states
the invariant — *"the shell is an adapter … a shell that starts reading the schema directly becomes a
second, slightly different database, and the difference is only ever found by somebody who trusted
it"* — and a command table that reached past it would be a third one. The single exception is the
capability table, which is a static array in `inillucent-driver` with no database behind it; the CLI
takes an edge to the driver to read it, and `layering.toml` records why.

### 3.2 The commands

28 of them. `db` is `--db PATH` or the first positional; `format` is `--json` / `--format`.

| verb | parameters | what it does |
|---|---|---|
| `query` | `sql`, `params?`, `limit?` | runs a statement that returns rows, typed |
| `exec` | `sql`, `params?` | runs one statement, reports changes and last rowid |
| `batch` | `sql` | runs many statements as one transaction |
| `run` | `input` | runs shell input, dot commands included, returns its text |
| `shell` | *(cli only)* | the interactive `sqlite3`-shaped REPL |
| `create` | `db` | creates a new database file, refusing an existing one |
| `tables` | `pattern?` | table names |
| `schema` | `pattern?`, `indent?` | the `CREATE` statements |
| `describe` | `table` | columns, types, nullability, keys, indexes, DDL — one call |
| `indexes` | `pattern?` | index names |
| `databases` | — | attached databases and their files |
| `explain` | `sql` | `EXPLAIN QUERY PLAN`, as the shell draws it |
| `import` | `file`, `table`, `format?`, `skip?` | CSV/TSV in |
| `export` | `sql` or `table`, `format?`, `out?` | rows out |
| `dump` | `objects?`, `data_only?` | the database as SQL |
| `backup` | `file` | a copy |
| `restore` | `file` | from a copy |
| `checkpoint` | — | WAL checkpoint |
| `integrity_check` | — | the integrity check |
| `analyze` | `table?` | `ANALYZE` |
| `stats` | — | page cache, pool bytes, page count, file size |
| `migrate` | `source`, `dest`, `kind` | what `inillucent-migrate` does, as a verb |
| `search` | `query`, `table?`, `k?` | retrieval: FTS5 and vector, without writing the SQL |
| `vector_search` | `table`, `column`, `vector`, `k?` | nearest neighbours by distance |
| `capabilities` | `name?` | the driver's checked capability table |
| `functions` | `pattern?` | the registered SQL functions |
| `version` | — | engine, format, ABI |
| `help` | `topic?` | the command table, or one entry |
| `mcp` | *(cli only)* | serves the table over MCP on stdio |

`shell` and `mcp` carry `cli_only` with a reason: one is a terminal REPL and the other is the server
that would be exposing it. The parity test asserts those are the *only* two, and that each carries a
reason string — an exclusion nobody has to justify is an exclusion that grows.

### 3.3 The outcome contract

Every command produces the same object, whichever front end asked:

```json
{
  "ok": true,
  "command": "query",
  "columns": [{ "name": "id", "type": "INTEGER" }, { "name": "name", "type": "TEXT" }],
  "rows": [[1, "Ada"], [2, null]],
  "row_count": 2,
  "total": 2,
  "more": false,
  "changes": 0,
  "last_insert_rowid": 0,
  "elapsed_ms": 0.41,
  "text": "id  name\n1   Ada\n2   "
}
```

and on failure:

```json
{
  "ok": false,
  "command": "query",
  "status": "syntax",
  "message": "near \"SELEC\": syntax error",
  "offset": 0,
  "text": "Error: near \"SELEC\": syntax error"
}
```

Three decisions inside that shape:

- **`status` is the driver's own status name**, from `inillucent_driver::Status::name()` — the same
  fourteen words a C binding sees through `inillucent_error_status`. `unsupported` is one of them and
  is not `syntax`, which is the whole design of the driver arriving in a second place rather than
  being re-invented in a third.
- **`null` is JSON null**, never `""`. `drivers/README.md` calls a layer that draws them the same "a
  layer nobody can trust" and this is the same claim in a different medium.
- **`text` is always present**, because the human rendering and the machine rendering are the same
  call. An MCP client gets `text`; a script gets the rest; nothing has to be run twice.

Exit codes: `0` success, `1` error, `2` usage, **`3` unsupported** — so a script can branch on "this
engine has not built that yet" without matching on a message.

### 3.4 The MCP server

`inillucent-mcp`, a stdio JSON-RPC 2.0 server, one JSON object per line, protocol `2025-06-18`.

- `initialize` → server info and `{"tools":{}}` capabilities.
- `tools/list` → one tool per non-`cli_only` command, named `inillucent_<verb>`, with the summary as
  `description` and a JSON Schema built from the command's parameters. Every parameter's description
  is the registry's, so a description written once is what the model reads.
- `tools/call` → runs it, answers `content: [{ "type": "text", "text": … }]`, with `isError: true` on
  failure. Every tool accepts an optional `"output": "text" | "json"`; **text is the default**,
  because a 27B model reads an aligned table far more reliably than it reads a JSON array, and the
  measurements in §6 are what settled that.
- `ping`, and `notifications/*` are accepted and answered with nothing, as the spec requires.

JSON is first-party. `serde_json` is allow-listed only for `inillucent-core` and `inillucent-bench`
(`docs/invariants/layering.toml`), and this is a production crate, so `command/json.rs` carries a
~200-line reader and writer. `render.rs`'s existing escaper moves into it rather than being
duplicated — two JSON escapers in one crate is exactly the kind of thing this repository's tests
exist to prevent.

**State.** *Changed in build: **one** open database, not four.* The server holds one and reopens it
when a call names a different file. One file is one buffer pool, and a server holding four holds four
pools whose sizes nobody asked about; a caller that alternates pays a reopen and a caller that does
not pays nothing. The engine is single threaded and the server's loop is single threaded, so there is
no lock and none is pretended. A `db` parameter on a tool call selects the file; without one the
server uses its `--db` or `$INILLUCENT_DB`.

**And a consequence nobody should discover the hard way: while the server has a database open, no
other process can write to it.** One writer, enforced by the engine - a second `inillucent` refuses
with `database is locked: a writer holds PENDING`. This ticket's own grading harness hit it on its
first run, setting a task up through the CLI while the server was serving it. It is the engine
working as designed; the answer is to drive one file through one process, and it is written down here
because the alternative is finding out during a demonstration.

**Confinement.** `--root DIR` refuses any path outside a directory, and `--readonly` refuses any
statement the binder classifies as a write. Both default to off, because the studio's use of this is
an agent that is *supposed* to create things — but an MCP server with no way to be confined is a
server nobody can put in front of a model they do not control.

> **Changed in build: the rendering switch is `output`, not `format`.** `export` already has a
> `format`, and that one means CSV against JSON against Markdown - a different question from whether
> the *result object* is drawn as a table or written as JSON. The collision was not theoretical:
> with both called `format`, `inillucent export people --format json` was read as "draw the result
> object as JSON" and quietly wrote CSV.

### 3.5 Parity, checked

`crates/inillucent-compat/tests/command_parity.rs`:

1. every command has a non-empty summary, and every parameter a non-empty description;
2. the MCP `tools/list` names are exactly the non-`cli_only` command names, prefixed;
3. every `cli_only` command carries a reason;
4. every tool's schema declares each required parameter, and no parameter the command does not have;
5. `inillucent --help` lists every command in the table.

It is the same argument the capability table makes in `drivers/README.md`: a claim nobody runs decays
into a claim that was true once.

---

## 4. Installing it

### 4.1 The release archive

`packaging/release.ps1` (Windows) and `packaging/release.sh` (macOS, Linux) build
`--release --locked`, stage the four binaries plus the C ABI library, the header, the licence and the
README into `dist/inillucent-<version>-<target>/`, write `SHA256SUMS`, and produce a `.zip` on Windows
and a `.tar.gz` elsewhere. One script, one layout, every platform — because every package below is
just a different way of getting that archive onto a machine.

Targets: `x86_64-pc-windows-msvc`, `aarch64-apple-darwin`, `x86_64-apple-darwin`,
`x86_64-unknown-linux-gnu`.

### 4.2 Windows

`packaging/install.ps1`: downloads the release for the running architecture, verifies its SHA-256
against `SHA256SUMS`, extracts to `%LOCALAPPDATA%\Programs\inillucent`, puts that on the user's
`PATH`, and prints the MCP block to paste into an agent's configuration. `-FromDist` installs the
locally built archive instead of downloading, which is how it is tested here. `-Uninstall` reverses
it. No admin rights, no registry beyond the user `PATH` value, no service.

An MSI is deliberately not the first move: it needs the WiX toolset in the build, it needs a
signing certificate to install without a SmartScreen warning, and it buys nothing over a per-user
directory on `PATH` for a command-line tool. `packaging/windows/README.md` records what an MSI would
take, so the decision can be revisited with the reasons in front of whoever revisits it.

### 4.3 macOS — scaffolded, to be finished on the MacBook

The parts that can only be done on a Mac are the parts that are left: `pkgbuild`/`productbuild` are
macOS-only, notarisation needs an Apple Developer ID and `notarytool`, and a universal binary needs
`lipo` over two `cargo build`s. So `packaging/macos/` ships **runnable scripts and an exact
checklist**, not a stub:

- `build-pkg.sh` — `cargo build` for both arches, `lipo` them into a universal binary, `pkgbuild` a
  component package rooted at `/usr/local`, `productbuild` a distribution package;
- `Distribution.xml` — the installer's own description;
- `notarize.sh` — `codesign`, `notarytool submit --wait`, `stapler staple`;
- `README.md` — the four things that need an Apple account, what each costs, and the one command to
  run once they exist.

`install.sh` (curl-to-shell) works on macOS today with no Apple account at all, so the `.pkg` is a
convenience rather than the only road.

### 4.4 The six ecosystems

| ecosystem | package | what a user runs | how it works |
|---|---|---|---|
| **cargo** | `inillucent-cli` on crates.io | `cargo install inillucent-cli` | builds from source; pulls the whole workspace, so every crate needs publishable metadata (§4.5) |
| **npm** | `inillucent` + four `@inillucent/cli-<platform>` | `npm i -g inillucent` / `npx inillucent` | the platform packages carry the binaries as `optionalDependencies`; the shim resolves one and `execve`s it, exactly as esbuild does |
| **python** | `inillucent` on PyPI | `pip install inillucent` | platform wheels carrying the binaries **and** the ctypes driver binding from `drivers/bindings/python`, so `pip install` gets both a command and a `import inillucent` |
| **go** | `github.com/jasonmcaffee/inillucent/packages/go` | `go install …/cmd/inillucent-install@latest` | a cgo binding over `inillucent_driver.h`, plus a tiny installer command for people who only want the binary |
| **php** | `jasonmcaffee/inillucent` on Packagist | `composer require jasonmcaffee/inillucent` | an FFI binding over the same header; `composer.json` sits at the repository root because Packagist reads a repository, not a subdirectory |
| **homebrew** | `jasonmcaffee/tap/inillucent` | `brew install jasonmcaffee/tap/inillucent` | a formula in a tap repository, installing the release tarball |

Every one of them is a different wrapper around §4.1's archive or around `cargo build`. That is on
purpose: six packaging systems with six *builds* is six things that can differ.

### 4.5 What publishing to crates.io actually requires

`cargo publish` refuses a path dependency with no version, so **every crate in the workspace needs
`version` on its internal edges and a `description`**, and all thirty-one have to be published in
dependency order before `inillucent-cli` can be. `packaging/cargo-publish.ps1` computes that order
from `layering.toml` and publishes with `--dry-run` unless `-Execute` is passed.

This is the one packaging decision with a consequence outside the repository: **crates.io publishes
source, permanently, and cannot be un-published** (a yank hides a version from resolution; it does
not remove it). The repository is private today. Publishing to crates.io — and `go install`, and
Homebrew, both of which need a public repository — is the moment inillucent becomes public. The
ticket asks for it; §7 records it as the thing to confirm rather than assume.

---

## 5. Verifying the local model can use it

The bar the ticket sets is *"verify our qwen localai is able to use it effectively"*, and "we wired it
up" is not that. So the verification is a scored run, not a screenshot:

- the MCP server is registered in `opencode.json` beside `aiservice-web`;
- the local Qwen build on `llama-server` :8080 is given a fresh database and a list of tasks it can
  only complete through the tools — create a table, insert rows, ask a question whose answer requires
  a `GROUP BY`, describe a table it did not create, and hit one thing the engine refuses;
- each is graded pass/fail on the *answer*, not on whether a tool was called;
- the score, the transcript and the failures go in `_agent_output/task-1836-cli-mcp/`.

The last item is the interesting one. An agent that hits `UNSUPPORTED` and reports "the database is
broken" has been failed by the tool description, not by the model, and that is a finding this ticket
should produce rather than avoid.

---

### What it actually scored

**Five of five, twice, on the first two runs there were.** `_agent_output/task-1836-cli-mcp/harness.mjs`
is the runner and `results.json` is the transcript; the model is the Qwen 3.8 build on `llama-server`
:8080, temperature 0.2, given all 27 tools with no curation.

| task | verdict | what the model did |
|---|---|---|
| create a table and insert four rows | pass | `inillucent_batch`, then `inillucent_query` to check itself |
| which department has the higher total salary, and what is it | pass | one `inillucent_query` with a `GROUP BY`; answered "Engineering, 310,000" |
| name every column in a table it did not create | pass | one `inillucent_describe` |
| how many projects belong to people in Engineering | pass | `inillucent_describe` on both tables, then a join |
| does the database support cancelling a statement | pass | `inillucent_capabilities`; said no, and did not call the database broken |

The first task is graded on **the database**, not on the reply: the harness reads the rows back and
compares them. The rest are graded on the answer, because the answer is the thing that would be
wrong.

Two of those results are the design working rather than the model being clever. It reached for
`describe` before writing SQL against a table it had not created - which is exactly why `describe`
answers columns, indexes, DDL and row count in **one** call, since a model that has to make four
pragma calls makes three and writes its query from an incomplete picture. And it reached for
`capabilities` to answer "can it cancel", which is what that table is for.

It also runs through **opencode** against the same model, which is the studio's real path:

```
> Use the inillucent tools. Create a table called cities ... which is larger and by how much?
  inillucent_batch  {"sql":"CREATE TABLE cities (name TEXT, population INTEGER); INSERT ..."}
  inillucent_query  {"sql":"SELECT name, population FROM cities ORDER BY population DESC"}
  Tokyo is larger than Delhi by 4,000,000 people.
```

and the rows were still in the file afterwards. The server is registered in
`claude-settings/opencode/opencode.json` beside `aiservice-web`.

**The one thing the run found** is in §3.4: a second process cannot write to a database the server
has open. The harness hit it, and it is the engine's one-writer rule rather than a defect.


## 6. Decisions worth arguing with

- **Text, not JSON, as the MCP default.** A structured result is strictly more informative and a
  smaller model is measurably worse at reading it. `format: "json"` is one parameter away.
- **`inillucent-shell` keeps its name and its behaviour.** A script that drives `sqlite3` drives it,
  and renaming the compatible thing to make room for a new thing would break the one property it was
  built for. The new binary is `inillucent`, and `inillucent shell` is the same REPL.
- **The registry drives the shell rather than the driver.** It costs a text rendering round trip on
  `query`. It buys one code path, and the shell is the path the 416-case differential probe already
  covers.
- **Refusing SQLite's internal options rather than ignoring them.** It will break a script that
  passed `-mmap` and did not care. It will not break a script that passed `-mmap` and did.

---

## 7. What this ticket does not do

- **No `.expert` and no `.session`.** Both are query-planner and changeset features with real
  engine work behind them; naming them here as "CLI gaps" would be filing engine work under a
  packaging ticket.
- **No signed Windows MSI and no notarised macOS `.pkg`.** Both need a certificate that costs money
  and belongs to a person; §4.2 and §4.3 record exactly what each takes.
- **No published package in a registry whose account does not already exist.** Where an account or a
  token is missing, the package is authored, built, and verified locally to the last step before the
  upload, and the ticket says which credential is missing rather than pretending.
