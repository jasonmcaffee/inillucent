# inillucent

inillucent is an embedded SQL database. It speaks SQLite's dialect on its own storage, and it has
keyword search and vector search built in. One `.rdb` file holds the tables and the search indexes.

This package gives Python two ways to use it:

- **The driver**: `Database`, `Connection` and `Rows`. It calls the inillucent C library inside your
  Python process through `ctypes`. Use the driver in an application.
- **The command line helpers**: `run()` and `query()`. Each call starts the `inillucent` program and
  reads the JSON it prints. Use them in scripts, for commands the driver has no method for.

## Install

```sh
pip install inillucent
```

The package needs Python 3.9 or later. The wheel for your platform holds the C library and the four
programs, so the install needs no compiler, no Rust toolchain and no network access after the
download.

| Platform | Wheel tag |
|---|---|
| Windows, x64 | `win_amd64` |
| macOS 13 or later, Apple silicon and Intel | `macosx_13_0_universal2` |
| Linux, x64 | `manylinux_2_28_x86_64` |
| Linux, arm64 | `manylinux_2_28_aarch64` |

The driver loads the C library from the wheel's `_lib` folder. Set `INILLUCENT_DRIVER_LIB` to the
path of a different library file to load that one. The helpers run the programs in the wheel's
`_bin` folder. A source install has neither folder. It needs a library you built with
`cargo build -p inillucent-driver-capi`, named by `INILLUCENT_DRIVER_LIB`.

## A first program

```python
from inillucent import Database

with Database("app.rdb") as database:
    connection = database.connect()
    connection.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)")
    connection.execute("INSERT INTO people VALUES (?1, ?2)", [1, "Ada"])
    rows = connection.execute("SELECT id, name FROM people", limit=200)
    print(rows.rows, rows.total)   # [[1, 'Ada']] 1
```

`Database("app.rdb")` creates the file when it does not exist. Leaving the `with` block closes every
connection, writes everything to the file and closes it.

`?1`, `?2` and so on are bound to the values in the list, in order. Bind values this way instead of
pasting them into the SQL text.

`limit` caps the rows returned. `rows.total` still counts every row the statement produced, and
`rows.more` is `True` when rows were left out. With no `limit`, `execute()` returns every row.

### Transactions

```python
with connection.transaction() as transaction:
    transaction.execute("INSERT INTO people VALUES (2, 'Grace')")
    transaction.execute("UPDATE people SET name = 'Ada Lovelace' WHERE id = 1")
```

Leaving the block normally commits. Leaving it with an exception rolls back. A statement that fails
inside `Transaction.execute()` rolls back the whole transaction before it raises.

### Statements you run many times

```python
with connection.prepare("INSERT INTO people VALUES (?1, ?2)") as statement:
    for key, name in [(3, "Grace"), (4, "Edsger")]:
        statement.execute([key, name])
```

### Values

| Python value | Bound as | Read back as |
|---|---|---|
| `None` | `NULL` | `None` |
| `bool` | `INTEGER` 0 or 1 | `int` |
| `int` | `INTEGER` | `int` |
| `float` | `REAL` | `float` |
| `str` | `TEXT` | `str` |
| `bytes`, `bytearray`, `memoryview` | `BLOB` | `bytes` |

Any other type raises `TypeError`.

### Threads

The engine is single threaded, and the driver has no lock inside. Use a `Database` and everything
opened from it on one thread, or serialize every call yourself. `Connection.cancel()` is the one call
meant to come from another thread.

## Errors

```python
from inillucent import DriverError, Unsupported

try:
    connection.execute("SELECT * FROM absent")
except Unsupported as why:
    print("not built yet:", why.feature)
except DriverError as why:
    print(why.status_name, why.message)   # not_found no such table: absent
```

Every refusal raises `DriverError`, also exported as `Error`. `DriverError` has these attributes:

| Attribute | What it holds |
|---|---|
| `status` | the status as a number |
| `status_name` | the status as text, such as `not_found` |
| `message` | the error text |
| `feature` | the missing feature, when the status is `unsupported` |
| `detail` | more detail, when the engine gives it |
| `offset` | the byte offset in the SQL where the error is, or `None` |

The thirteen status names are `unsupported`, `syntax`, `not_found`, `constraint`, `readonly`,
`busy`, `interrupted`, `corrupt`, `io`, `full`, `too_big`, `invalid_state` and `internal`.

`Unsupported` is the subclass raised for status `unsupported`. It means the engine has not built
that feature. The SQL is not wrong, and a different spelling fails the same way. Catch
`Unsupported` before `DriverError` so the two cases stay apart. Call `capabilities()` before you
write an unusual statement.

## From the command line

`pip install` puts four programs on `PATH`:

| Program | What it is |
|---|---|
| `inillucent` | the command line: 30 commands, each with `--output json` |
| `inillucent-shell` | an interactive shell that works like `sqlite3`, with 63 of its 65 dot commands |
| `inillucent-mcp` | an MCP server: 28 of the same commands served to an AI agent |
| `inillucent-migrate` | builds a database from a legacy retrieval index. `inillucent migrate` copies a SQLite file or a PostgreSQL or MySQL database |

```sh
inillucent create notes.rdb
inillucent --db notes.rdb exec "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"
inillucent --db notes.rdb query "SELECT * FROM notes"
inillucent --db notes.rdb describe notes
inillucent-shell notes.rdb
inillucent help
```

The `inillucent` program exits with code 0 on success, 1 on a failure, 2 when the command line
cannot be read, and 3 when the engine has not built a feature.

### The command line helpers in Python

```python
from inillucent import run, query

print(run("describe", db="notes.rdb", table="notes")["ddl"])
print(query("SELECT * FROM notes WHERE id > ?1", db="notes.rdb", params=[3]))
```

| Helper | What it does |
|---|---|
| `run(command, db, **arguments)` | runs one `inillucent` command with `--output json` and returns the result as a `dict`. `table="notes"` becomes `--table notes`. A refusal is returned with `ok` set to `False`. It raises `RuntimeError` only when the program could not be run |
| `query(sql, db, params, limit)` | runs one query and returns its rows as a list of `dict` keyed by column name. Raises `DriverError` or `Unsupported` on a refusal |
| `binary(program)` | returns the path of one of the four programs in the wheel |

Each helper call starts a process, so use the driver for anything that runs often. `query()` returns
at most 200 rows, the default of the `query` command. Pass `limit=0` for every row.

## For an AI agent

MCP, the Model Context Protocol, is how an AI agent calls tools. Add `inillucent-mcp` to an MCP
client's configuration:

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

`inillucent-mcp` serves 28 of the command line's commands as MCP tools. The tools are generated from
the same command table as the command line. `--readonly` refuses every statement that changes data.
`--root DIR` refuses every path outside `DIR`.

## The API

These are the driver's classes and functions. `cargo test -p inillucent-compat --test tooling documentation::`
reads this table and fails if a name in the first column is not declared in the driver source.

| what | one line |
|---|---|
| `Database(path, create=True, read_only=False, diagnostics=False)` | opens a database file. `create=True` makes the file when it is missing. |
| `Database.connect()` | returns a new `Connection`. Each connection is its own session, and `temp.` tables and `ATTACH` belong to one session. |
| `Database.path` | a property: the path of the database file. |
| `Database.checkpoint()` | copies everything written so far into the database file. |
| `Database.integrity_check()` | checks every table and index, and raises on the first problem. |
| `Database.backup_to(path)` | copies the database to `path`, then opens and checks the copy. |
| `Database.close()` | closes every connection, writes everything to the file and closes it. Leaving a `with` block calls it. |
| `Connection.execute(sql, params, limit)` | runs one statement and returns `Rows`. |
| `Connection.execute_batch(sql)` | runs several statements separated by semicolons, for their effect. |
| `Connection.prepare(sql)` | compiles a statement and returns a `Statement` you can run many times. |
| `Connection.transaction()` | begins a transaction and returns a `Transaction`. |
| `Connection.last_insert_rowid` | a property: the rowid the last `INSERT` assigned. |
| `Connection.total_changes` | a property: how many rows every statement on this connection has changed. |
| `Connection.in_transaction` | a property: whether a transaction is open. |
| `Connection.schema_cookie` | a property: a number that changes whenever the schema changes. |
| `Connection.cancel()` | asks the running statement to stop, from another thread. It stops at the next check, so the stop is not instant. |
| `Connection.close()` | closes the connection. |
| `Statement.execute(params, limit)` | binds the values, runs the statement and returns `Rows`. |
| `Statement.close()` | frees the compiled statement. |
| `Transaction.execute(sql)` | runs one statement inside the transaction and returns the number of rows it changed. |
| `Transaction.commit()` | commits the transaction. |
| `Transaction.rollback()` | rolls the transaction back. |
| `Rows` | one result: `columns`, `column_types`, `rows`, `total`, `more`, `affected`, `elapsed_us` and `tag`. Supports `len()`, iteration and indexing by row number. |
| `capabilities()` | returns every capability the engine declares, as a list of `dict` with `name`, `supported` and `note`. |
| `supports(name)` | returns 1 for yes, 0 for no, -1 for partial, and -2 for a name this build does not know. Treat -2 as no. |
| `abi_version()` | returns the version of the C library's interface, such as `1.0.0`. |
| `DriverError` | the exception for every refusal. Also exported as `Error`. |
| `Unsupported` | the subclass of `DriverError` for a feature the engine has not built. |

`inillucent.__version__` is the package version.

## More

- [Getting started](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/getting-started.md)
- [SQL support](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/sql.md)
- [Vector and keyword search](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/vector-search.md)
- [The C library and language bindings](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/drivers/README.md)
- [Glossary](https://github.com/Black-Rainbow-Labs/Inillucent/blob/main/docs/glossary.md)

MIT licence. Source: <https://github.com/Black-Rainbow-Labs/Inillucent>
