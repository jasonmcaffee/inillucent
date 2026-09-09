---
name: inillucent-mcp
description: Serve an inillucent database to an AI agent over MCP, with --readonly and --root confinement, and read the results correctly. Use when asked to give an agent access to a database, wire inillucent into Claude Code or another MCP client, or debug an MCP tool call against it.
---

# Giving an agent a database, over MCP

`inillucent-mcp` serves 27 of the CLI's commands as MCP tools over standard input and output. They
are **generated from the same command table the CLI reads**, so the two cannot drift — a test
(`command_parity.rs`) fails the build if they do — and a tool's description is the same sentence
`inillucent help <command>` prints.

## Wiring it up

```json
{
  "mcpServers": {
    "inillucent": {
      "command": "inillucent-mcp",
      "args": ["--db", "app.rdb"]
    }
  }
}
```

Tools arrive named `inillucent_query`, `inillucent_exec`, `inillucent_describe`,
`inillucent_search`, `inillucent_migrate`, and so on.

## The two flags to think about before you hand it over

```json
"args": ["--db", "app.rdb", "--readonly", "--root", "/srv/data"]
```

- **`--readonly` refuses every statement that changes something.** The classification is the
  *binder's* — whether the statement binds as a query — not a scan of the text, so
  `SELECT … ; DROP TABLE …` does not slip through and a `SELECT` that happens to contain the word
  "delete" is not refused.
- **`--root DIR` refuses every path outside a directory.** The check is lexical, after normalising
  `..`, and it happens **before** the file is opened — a check that resolved the path through the
  file system would have to create it first, and a confinement that has already touched the disk is
  not one.

**`--root` also refuses a migration from a server.** `migrate --kind postgres` dials a host and a
port, and the confinement is about *reach*, not only about paths — a verb that could open a socket
would be a hole straight through it. That refusal is by name, so an agent that hits it is told why
rather than left guessing.

Neither flag is a substitute for filesystem permissions. They are the difference between "this agent
can read the reporting database" and "this agent can read everything the user can", which is usually
the difference you wanted.

## Making a call

```json
{ "name": "inillucent_query",
  "arguments": { "sql": "SELECT id, body FROM note WHERE created > ?1",
                 "params": ["2026-01-01"], "limit": 50 } }
```

- **`params` binds `?1`, `?2` … in order.** Pasting values into `sql` is how an agent produces a
  quoting bug it cannot see.
- **`limit` caps the rows returned, not the count.** The result's `total` is the real number and
  `more` says whether anything was cut.
- The result is the same JSON object every surface produces: `ok`, `columns` with observed storage
  classes, `rows`, `total`, `more`, `elapsedMs`, and on a failure `status` plus `message`.

## The order that avoids wasted calls

1. `inillucent_capabilities` — what this engine does, checked against the running engine in both
   directions by a test. Ask before composing anything unusual.
2. `inillucent_tables` — what is here.
3. `inillucent_describe` — the one table, with its columns, keys, indexes, row count and DDL. **Do
   this before writing SQL against a table you did not create.**
4. `inillucent_query`, with bound parameters.

## Reading a failure

`status` is one of the driver's fourteen names, and it is what to branch on rather than the message:

| status | what to do |
|---|---|
| `unsupported` | **the engine has not built that construct.** Not a typo. Rewording will not help — pick another construct, or ask `capabilities` |
| `syntax` | the statement did not parse; the message carries the byte offset |
| `constraint` | the data was refused, and the message names the constraint |
| `invalid_state` | the request was not one the surface could carry out — a confined path, a destination that already exists, a write on a read-only server |
| `not_found` | the file, table or object is not there |

## What is not served, and why

`shell` is CLI-only: it reads a keyboard and writes a screen, and neither exists at the other end of
an MCP call. Use `inillucent_run` instead, which drives the same shell over a pipe and hands back
what it printed — dot commands included.
