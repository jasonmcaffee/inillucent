---
name: inillucent-mcp
description: Serve an inillucent database to an AI agent over MCP, with --readonly and --root confinement, and read the results correctly. Use when asked to give an agent access to a database, wire inillucent into Claude Code or another MCP client, or debug an MCP tool call against it.
---

# Giving an agent a database, over MCP

`inillucent-mcp` serves 28 of the CLI's commands as MCP tools over standard input and output. They
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
  `inillucent_run` is served like any other tool: the verb is admitted and the shell
  underneath refuses each statement that writes, so an agent on a read only server still
  reaches every dot command.
- **`--root DIR` refuses every path that *resolves* outside a directory.** Resolves, not spells:
  every component is followed through the file system as it is appended, so a Windows junction or a
  Unix symbolic link placed below the root is replaced by what it points at before the check
  happens. A path that does not exist yet stops the resolution at its deepest existing ancestor,
  which is what lets the same check authorise a file about to be created, and the VFS checks the
  target again at the moment it is opened.

  The policy covers **every** file the request causes to be opened, not only the one in the `db`
  argument: `create`, `import`, `export`, `backup`, `restore`, `migrate`, the database the server
  was started on, and the two statements that name a file of their own, `ATTACH DATABASE` and
  `VACUUM INTO`. It is enforced in the VFS, which is the only thing in the workspace that opens a
  file, so a command added later is confined without anybody remembering to add it to a list.
  Temporary files a confined process makes are made inside the root.

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
  classes, `rows`, `row_count`, `total`, `more`, `changes`, `last_insert_rowid`, `elapsed_ms`, `text`,
  and on a failure `status` plus `message`.

## The order that avoids wasted calls

1. `inillucent_capabilities` — what this engine does, checked against the running engine in both
   directions by a test. Ask before composing anything unusual.
2. `inillucent_tables` — what is here.
3. `inillucent_describe` — the one table, with its columns, keys, indexes, row count and DDL. **Do
   this before writing SQL against a table you did not create.**
4. `inillucent_query`, with bound parameters.

## Reading a failure

`status` is one of the driver's thirteen names, and it is what to branch on rather than the message:

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
