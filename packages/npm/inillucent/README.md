# inillucent

inillucent is an embedded SQL database. It speaks SQLite's dialect on its own storage, and it has
keyword search and vector search built in. One `.rdb` file holds the tables and the search indexes,
and one process reads and writes it.

This npm package installs the four inillucent programs and a small JavaScript API that runs them.

## Install

```sh
npm install -g inillucent
```

Or run a program once without installing it:

```sh
npx inillucent help
```

The package needs Node 18 or later. There is no install script and nothing downloads at install
time. The programs come in one extra package per platform, listed as optional dependencies. npm
installs only the one that matches your machine, so `npm ci` works offline and behind a registry
proxy.

| Platform | Package npm installs |
|---|---|
| Windows, x64 | `@blackrainbowlabs/cli-win32-x64` |
| macOS, Apple silicon | `@blackrainbowlabs/cli-darwin-arm64` |
| macOS, Intel | `@blackrainbowlabs/cli-darwin-x64` |
| Linux, x64 | `@blackrainbowlabs/cli-linux-x64` |
| Linux, arm64 | `@blackrainbowlabs/cli-linux-arm64` |

Each platform package holds the four programs in `bin/`, the C library in `lib/`, and its header
`inillucent_driver.h` in `include/`. If you installed with `--no-optional`, install the platform
package by name, for example `npm install @blackrainbowlabs/cli-linux-x64`.

## The four programs

| Program | What it is |
|---|---|
| `inillucent` | the command line: 30 commands, such as `query`, `exec`, `describe`, `import`, `export` and `search` |
| `inillucent-shell` | an interactive shell that works like `sqlite3`, with 63 of its 65 dot commands |
| `inillucent-mcp` | an MCP server: 28 of the same commands served to an AI agent |
| `inillucent-migrate` | builds a database from a legacy retrieval index. `inillucent migrate` copies a SQLite file or a PostgreSQL or MySQL database |

## From a shell

```sh
inillucent create app.rdb
inillucent --db app.rdb exec "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"
inillucent --db app.rdb exec "INSERT INTO notes (body) VALUES (?1)" --params '["hello"]'
inillucent --db app.rdb query "SELECT * FROM notes"
inillucent --db app.rdb describe notes
inillucent help
```

`--params` binds `?1`, `?2` and so on, in order. Add `--output json` to any command to get a JSON
object a program can parse.

| Exit code | Meaning |
|---|---|
| `0` | success |
| `1` | the command failed |
| `2` | the command line could not be read, such as an unknown command |
| `3` | the engine has not built that feature. Rewording the SQL does not help |

## From JavaScript

```js
import { query, inillucent } from 'inillucent';

await inillucent('exec', { db: 'app.rdb', sql: 'INSERT INTO notes (body) VALUES (?1)', params: ['world'] });

const rows = await query('SELECT id, body FROM notes WHERE id > ?1', { db: 'app.rdb', params: [1] });
// [ { id: 2, body: 'world' } ]

const described = await inillucent('describe', { db: 'app.rdb', table: 'notes' });
console.log(described.ddl, described.indexes, described.row_count_in_table);
```

Each call starts one `inillucent` process with `--output json` and parses the object it prints.
`params` travels to the process on standard input, so a large value does not hit the operating
system's limit on command line length.

Starting a process costs time on every call. This package suits a build script or a tool wrapper
that makes a dozen calls. It does not suit a loop over a million rows. For that, write a binding over
the C library in the platform package. The
[driver guide](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/drivers/README.md)
explains how.

Because each call is its own process, this package has no connection object and no transaction that
spans two calls. To run several statements as one transaction, use the `batch` command:
`inillucent('batch', { db, sql: 'INSERT ...; UPDATE ...' })`.

### Values

| JavaScript value | Bound as |
|---|---|
| `null` or `undefined` | `NULL` |
| a number | `INTEGER` or `REAL`. `-0` keeps its sign |
| `NaN`, `Infinity` | refused with a `TypeError`, because SQL has no value for them |
| a string | `TEXT` |
| a `Uint8Array` | `BLOB` |

`query()` returns a BLOB column as a `Uint8Array`, so bytes read by one query can be bound into the
next.

## Errors

```js
const result = await inillucent('query', { db: 'app.rdb', sql: 'SELECT * FROM absent' });
// result.ok === false, result.status === 'not_found', result.message === 'no such table: absent'

try {
  await query('SELECT * FROM absent', { db: 'app.rdb' });
} catch (error) {
  console.log(error.status); // 'not_found'
}
```

`inillucent()` returns a refusal as its result object with `ok: false`. `query()` throws an `Error`
with three extra fields: `status`, `message` and `feature`. Both throw only when the program could
not be run at all.

`status` is one of thirteen names: `unsupported`, `syntax`, `not_found`, `constraint`, `readonly`,
`busy`, `interrupted`, `corrupt`, `io`, `full`, `too_big`, `invalid_state` and `internal`.

`unsupported` means the engine has not built that feature, and `feature` names it. The SQL is not
wrong, and a different spelling fails the same way. Check `inillucent capabilities` before you
write an unusual statement.

## For an AI agent

MCP, the Model Context Protocol, is how an AI agent calls tools. Add `inillucent-mcp` to an MCP
client's configuration:

```json
{
  "mcpServers": {
    "inillucent": {
      "command": "npx",
      "args": ["-y", "-p", "inillucent", "inillucent-mcp", "--db", "app.rdb"]
    }
  }
}
```

`inillucent-mcp` serves 28 of the command line's commands as MCP tools. The tools are generated from
the same command table as the command line. `--readonly` refuses every statement that changes data.
`--root DIR` refuses every path outside `DIR`.

## The API

`cargo test -p inillucent-compat --test tooling documentation::` reads this table and fails if a name in the
first column is not declared in `index.mjs` or `resolve.mjs`.

| what | one line |
|---|---|
| `inillucent(command, options)` | runs one command and resolves to its parsed JSON result. `options.db` names the file. Every other key becomes a flag: `{ table: 'notes' }` is `--table notes`, `true` is a bare flag. |
| `query(sql, options)` | runs one query and resolves to its rows as objects keyed by column name. `options` takes `db`, `params` and `limit`. Throws on a refusal. |
| `resolveBinary(program)` | returns the path to one of the four programs on this machine. `INILLUCENT_BIN` overrides it. |
| `platformPackage()` | returns the platform package this machine needs, or `null` when there is none. |
| `PROGRAMS` | an object naming the four programs. |

A result object from `inillucent()` has these fields on success: `ok`, `command`, `columns`, `rows`,
`row_count`, `total`, `more`, `changes`, `last_insert_rowid`, `elapsed_ms` and `text`. Some commands
add their own, such as `ddl` and `indexes` from `describe`. `total` counts every row the statement
produced, even when `limit` cut the rows returned.

The `query` command returns at most 200 rows unless you pass `limit`. `limit: 0` returns every row.
`more: true` in the result means rows were left out.

`INILLUCENT_BIN` names an `inillucent` binary to use in place of the platform package. The other
three programs are then looked for in the same folder.

## More

- [Getting started](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/getting-started.md)
- [SQL support](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/sql.md)
- [Vector and keyword search](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/vector-search.md)
- [Glossary](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/glossary.md)

MIT licence. Source: <https://github.com/Black-Rainbow-Labs/Inillucent>
