# inillucent

An embedded SQL database that speaks SQLite's dialect on its own storage, with
full-text and vector search built in — the job of PostgreSQL + pgvector + an
embedding server, in one process and one file.

```sh
npm install -g inillucent
# or, without installing anything permanently:
npx inillucent help
```

No native build step and no postinstall download: the binaries ship as
per-platform packages that npm installs only where they run, so `npm ci` works
offline and behind a proxy.

## Four programs

| | |
|---|---|
| `inillucent` | the command line: `query`, `exec`, `describe`, `import`, `export`, `search`, and twenty more |
| `inillucent-shell` | an interactive shell shaped like `sqlite3`, with all 63 of its dot commands |
| `inillucent-mcp` | the same commands served to an AI agent over MCP |
| `inillucent-migrate` | builds an inillucent database from a SQLite file |

## From a shell

```sh
inillucent create app.rdb
inillucent --db app.rdb exec "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"
inillucent --db app.rdb exec "INSERT INTO notes (body) VALUES (?1)" --params '["hello"]'
inillucent --db app.rdb query "SELECT * FROM notes"
inillucent --db app.rdb describe notes
inillucent help
```

Exit codes carry meaning: `0` success, `1` failed, `2` a command line nobody
could act on, and **`3` a construct the engine has not built yet** — so a script
can branch on "not yet" without matching on a message.

## From Node

```js
import { query, inillucent } from 'inillucent';

const rows = await query('SELECT id, body FROM notes WHERE id > ?1', {
  db: 'app.rdb',
  params: [3],
});
// [{ id: 4, body: 'hello' }]

const described = await inillucent('describe', { db: 'app.rdb', table: 'notes' });
console.log(described.ddl, described.indexes, described.row_count_in_table);
```

Each call is a process, so this is the right tool for the dozen calls a build
script or a tool wrapper makes and the wrong one for a loop over a million rows.
For that, write a binding over the C ABI — the header ships in this package's
platform dependency under `include/`, and
[`drivers/README.md`](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/drivers/README.md)
is written to be followed.

## For an agent

```json
{
  "mcpServers": {
    "inillucent": {
      "command": "npx",
      "args": ["-y", "inillucent-mcp", "--db", "app.rdb"]
    }
  }
}
```

27 tools, generated from the same command table the CLI reads, so the two can
never drift. `--readonly` refuses every statement that changes something, and
`--root DIR` refuses every path outside a directory.

## Licence

MIT. Source: <https://github.com/Black-Rainbow-Labs/Inillucent>

## The API

Every method this binding has. The worked example each one appears in is the link; nothing here is
a summary of a method that does not exist, because
`cargo test -p inillucent-compat --test documentation` reads this table and fails on a name the
binding source does not declare.

| what | one line |
|---|---|
| `inillucent(command, options)` | run one command of the command line and return its parsed JSON. `options.db` names the file, `options.args` the rest. |
| `query(sql, options)` | run one `SELECT` and return its rows. `options.params` binds `?1`, `?2`; `options.limit` caps the rows kept. |
| `resolveBinary(program)` | the path to one of the four programs on this platform, from the platform package npm installed. |
| `platformPackage()` | the name of the platform package this machine needs, which is what an install failure should name. |
| `PROGRAMS` | the four programs and what each is for, as an object. |

Every call goes through the command line rather than through the C ABI: the four programs are what
the platform package ships, and `--output json` is the same object every other binding sees. That
is why there is no `Connection` here and no transaction - a command is one process, and a
transaction that spanned two of them would be a transaction nothing held open.
