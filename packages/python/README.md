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

## The API

Every method this binding has. The worked example each one appears in is the link; nothing here is
a summary of a method that does not exist, because
`cargo test -p inillucent-compat --test documentation` reads this table and fails on a name the
binding source does not declare.

| what | one line |
|---|---|
| `Database(path, **options)` | open or create a database file. |
| `Database.connect()` | a `Connection`. Each one is its own session, which is what `temp.` and `ATTACH` are scoped to. |
| `Database.path()` | the file this database is in. |
| `Database.checkpoint()` | make everything written so far durable in the file. |
| `Connection.execute(sql, params, limit)` | run one statement and return `Rows`. |
| `Connection.execute_batch(sql)` | run several statements separated by semicolons, for their effect. |
| `Connection.prepare(sql)` | a `Statement`, compiled once and run many times. |
| `Connection.transaction()` | a `Transaction`. Use it as a context manager: leaving the block without a commit rolls back. |
| `Connection.last_insert_rowid()` | the rowid the last `INSERT` assigned. |
| `Connection.total_changes()` | how many rows every statement so far has changed. |
| `Connection.in_transaction()` | whether a transaction is open. |
| `Connection.schema_cookie()` | the schema's generation, which changes when the schema does. |
| `Connection.cancel()` | stop the running statement, from another thread. |
| `Statement.execute(params, limit)` | run the compiled statement with these values. |
| `Transaction.execute(sql)` | run a statement inside the transaction. |
| `Transaction.commit()` | keep what the transaction wrote. |
| `Transaction.rollback()` | discard it. The same thing leaving the block does. |
| `Rows` | `len()`, iteration, and indexing by row. |
| `capabilities()` | what the engine does, as the checked table rather than a feature list. |
| `supports(name)` | whether one capability is answered, refused, or silent. |
| `version()` | the driver's version, and the engine's beneath it. |
| `abi_version()` | the C ABI version this binding links against. |
| `DriverError` | one failure, with its status and its message. |
| `Unsupported` | the subclass raised for a construct this engine has not built - a different thing from a syntax error, which is the point. |
