# inillucent

An embedded SQL database that speaks SQLite's dialect on its own storage, with
full-text and vector search built in — the job of PostgreSQL + pgvector + an
embedding server, in one process and one file.

```sh
pip install inillucent
```

No compiler, no Rust toolchain, no build step: the wheel carries the binaries
and the C ABI library for your platform.

## In process

```python
from inillucent import Database

with Database("app.rdb") as database:
    connection = database.connect()
    connection.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)")
    connection.execute("INSERT INTO people VALUES (?1, ?2)", [1, "Ada"])
    rows = connection.execute("SELECT id, name FROM people", limit=200)
    print(rows.rows, rows.total)   # [[1, 'Ada']] 1
```

`total` is **exact**, not an estimate: the engine materialises, so a grid can say
`1–200 of 4,317` and mean it.

### Unsupported is its own exception

```python
from inillucent import Database, Unsupported

try:
    connection.execute("SOMETHING NOT BUILT YET")
except Unsupported as why:
    print("not yet:", why.feature)   # an engine gap, not your typo
```

Catching `Unsupported` and `Error` together throws away the one distinction this
driver was designed around. An application needs to be able to say "this engine
cannot do that yet" rather than "check your spelling".

## From the command line

`pip install` puts four programs on `PATH`:

```sh
inillucent create app.rdb
inillucent --db app.rdb exec "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"
inillucent --db app.rdb query "SELECT * FROM notes"
inillucent --db app.rdb describe notes
inillucent-shell app.rdb          # the sqlite3-shaped REPL
inillucent help
```

and they are reachable from Python too, for the verbs the driver has no call for:

```python
from inillucent import run, query

print(run("describe", db="app.rdb", table="notes")["ddl"])
print(query("SELECT * FROM notes WHERE id > ?1", db="app.rdb", params=[3]))
```

## For an agent

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

27 tools, generated from the same command table the CLI reads. `--readonly`
refuses every statement that changes something; `--root DIR` refuses every path
outside a directory.

## Licence

MIT. Source: <https://github.com/Black-Rainbow-Labs/Inillucent>
