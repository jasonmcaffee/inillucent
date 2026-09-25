# AGENTS.md: working on the coffee shop service

This folder holds `coffee-server`, a REST API for a coffee shop's orders, stock and double entry
books, written in Rust, with one inillucent database file behind it. Read `README.md` first. It
explains the schema, the triggers, and the SQL behind each route, with what each returned.

## Running it

```sh
cargo run --release -- seed      # a week of demo trading in data/coffee.rdb, about 30 seconds
cargo run --release -- serve     # http://127.0.0.1:3000
cargo test --release             # the end to end tests, about 35 seconds once built
```

`serve --addr 127.0.0.1:0` picks a free port and prints it on the first line. `seed` refuses a
database that already has orders, so give it a new `--db`.

## Where to make a change

| You want to | Change |
|---|---|
| add or change a table, a constraint, a trigger or a view | `src/schema.rs`. The schema runs only when a file has no `orders` table, so delete `data/coffee.rdb` or use a new `--db` |
| change what a query returns | the method in `src/store/`. Every SQL statement is there or in `src/schema.rs`, and nowhere else |
| add a route | a handler in `src/routes/` that calls one store method, and a row in the route tables in `src/routes/mod.rs` and `README.md` |
| change how an error is answered | `src/error.rs` |
| post something new to the books | an entry through `post` in `src/store/inventory.rs`, or a trigger in `src/schema.rs`, and a new value in `journal_entry.source` |

## Rules this code follows

- **Money is an integer number of cents.** Every amount column ends in `_cents`. Round with integer
  arithmetic, `(x * rate_bp + 5000) / 10000`, and never through a `REAL`.
- **Every transaction that writes to the books calls `ensure_balanced` before it commits.** The
  `unbalanced_entry` view must be empty. The books are never updated or deleted: triggers refuse it,
  so a correction is a new entry.
- **Let the schema enforce what it can.** A rule that can be a `CHECK`, a `UNIQUE` index, a foreign
  key or a trigger goes in `src/schema.rs`, and the engine's `constraint` status becomes `409` in
  `src/error.rs`. Do not repeat the rule in Rust.
- **Bind every value.** A value goes in `?1`, `?2` and so on. A batch goes in as one JSON array and is
  read with `json_each` inside the SQL.
- **A change to more than one row is one transaction.** Use `self.begin()`, and let an early `?` drop
  the transaction, which rolls it back.
- **A report is the rows of one query**, returned through `Sql::objects`, so the README can show the
  query and its answer side by side.
- **Each test starts the real server** and checks the JSON it returns. Put new tests in
  `tests/e2e.rs`, date every event on `support::DAY` with an explicit time, and work the expected
  numbers out by hand in a comment.

## What to avoid in inillucent 1.0.30

These differ from SQLite in 1.0.30. `README.md` section 17 has the list with what the code does
instead.

- Do not name an aggregate after a column of a table in `FROM` and then `ORDER BY` it in a grouped
  query: `sum(quantity) AS quantity ... ORDER BY quantity` sorts by the table's column. Use a new
  name such as `sold`.
- Do not read one row from a view with `WHERE id = ?`. A view is never flattened, so every row of
  it is built first. Read the base tables.
- Do not pass one `json_each` row to another `json_each`, and do not join on
  `json_each.value ->> '$.field'`. Read the fields in a derived table and join on its columns.
- Do not put a window function in a derived table, a CTE or a view, and do not order a windowed query
  by an expression. Rank everything at the top level and pick or sort the rows in Rust.
- Do not put a correlated scalar subquery next to a window function in one `SELECT`. Join and group.

## If something fails

| You see | What it means |
|---|---|
| `409` with `CHECK constraint failed`, `UNIQUE constraint failed` or `FOREIGN KEY constraint failed` | the schema refused the write. The message names the rule |
| `409` with a sentence such as `lines can only be added to an open order` | a trigger refused the write with `RAISE(ABORT, ...)` |
| `500` with `a journal entry does not balance` | a change to the SQL that posts to the books is wrong. Nothing was saved |
| `501` with `unsupported` | inillucent has not built a construct the SQL uses. Rewrite the SQL; rewording it the same way does not help |
| every request takes a few milliseconds even when its queries are fast | `SharedDatabase` starts a new session for each statement. `README.md` section 18 has the numbers |
