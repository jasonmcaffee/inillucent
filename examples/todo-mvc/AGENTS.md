# AGENTS.md: working on the todo service

This folder holds `todo-server`, a REST API for todos written in Rust, with one inillucent database
file behind it. Read `README.md` first. It lists every route, the schema, and the SQL behind each
feature.

## Running it

```sh
cargo run --release -- seed      # demo data in data/todo.rdb
cargo run --release -- serve     # http://127.0.0.1:3000
cargo test --release             # the end to end tests, about a second once built
```

`serve --addr 127.0.0.1:0` picks a free port and prints it on the first line.

## Where to make a change

| You want to | Change |
|---|---|
| add or change a column, an index, a trigger or the `todo_card` view | `src/schema.rs`. The schema runs only when a file has no `todo` table, so delete `data/todo.rdb` or use a new `--db` |
| change what a query returns | the method in `src/store/`. Every SQL statement is there and nowhere else |
| add a route | a handler in `src/routes.rs` that calls one store method, and a row in the route tables in `src/routes.rs` and `README.md` |
| change how an error is answered | `src/error.rs` |

## Rules this code follows

- **Bind every value.** A value goes in `?1`, `?2` and so on. Only column names chosen by the code are
  written into SQL text, as `update_fields` in `src/store/todos.rs` does.
- **Let the schema enforce what it can.** A rule that can be a `CHECK`, a `UNIQUE` or a foreign key
  goes in `src/schema.rs`, and the engine's `constraint` status becomes `409` in `src/error.rs`. Do
  not repeat the rule in Rust.
- **A change to more than one row is one transaction.** Use `self.begin()`, and let an early `?` drop
  the transaction, which rolls it back.
- **Let the triggers keep `todo_fts` in step.** `SEARCH_TRIGGERS` in `src/schema.rs` writes the
  search entry on every insert, every change to `title` or `notes`, and every delete of a todo.
  Do not write `todo_fts` from the store.
- **Read cells by column name**, with `Record`, so adding a column to a `SELECT` cannot shift the
  others.
- **Each test starts the real server** and checks the JSON it returns. Put new tests in
  `tests/e2e.rs`, and fix any date that decides "overdue" with `?today=`.

## If something fails

| You see | What it means |
|---|---|
| `409` with `constraint failed` | the schema refused the write. The message names the rule |
| `501` with `unsupported` | inillucent has not built a construct the SQL uses. Rewrite the SQL; rewording it the same way does not help |
| `database disk image is malformed: neither meta page is readable` from the `inillucent` command line | the command line is older than 1.0.30 and cannot read the file. Install a newer one |
