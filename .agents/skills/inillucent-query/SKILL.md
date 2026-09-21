---
name: inillucent-query
description: Explore and query an existing inillucent database - list tables, describe a schema, run SELECTs safely, read the query plan, and export rows. Use when asked to look at, inspect, report on or extract data from an .rdb file you did not create.
---

# Querying a database somebody else built

The order below is the one that avoids wasted turns: find out what is there, look at the one table
you care about, *then* write SQL against it.

## 1. What is in this file

```sh
inillucent --db app.rdb tables            # tables and views
inillucent --db app.rdb schema            # every CREATE statement
inillucent --db app.rdb schema 'user%'    # ...matching a LIKE pattern
inillucent --db app.rdb indexes
inillucent --db app.rdb stats             # page cache, pool size, the shape of the file
```

## 2. Then the one table

```sh
inillucent --db app.rdb describe note
```

Columns with their types, the primary key, every index, the row count and the DDL — in one call.
**Do this before writing SQL against a table you did not create.** It also tells you whether a table
is a full-text or vector one, which changes how you query it (see
[`inillucent-search`](../inillucent-search/SKILL.md)).

## 3. Then the query

```sh
inillucent --db app.rdb query "SELECT id, body FROM note WHERE created > ?1 ORDER BY id" \
  --params '["2026-01-01"]' --limit 50 --output json
```

- **`--params` binds `?1`, `?2` … in order.** Use it. Pasting a value into the SQL text is how you get
  a quoting bug, and it is avoidable in one flag.
- **`--limit` cuts what is handed back, not what was counted.** The result's `total` is the real count
  and `more` says whether anything was cut. `--limit 0` means every row.
- **`--readonly` refuses every statement that would change something**, classified by the binder
  rather than by scanning the text. Put it on the command line whenever you are only reading — it
  turns "I meant to write a SELECT" into a refusal instead of an edit.

```sh
inillucent --db app.rdb --readonly query "SELECT count(*) FROM note"
```

## 4. When a query is slow

```sh
inillucent --db app.rdb explain "SELECT * FROM note WHERE created > '2026-01-01'"
```

The plan in SQLite's own `EXPLAIN QUERY PLAN` idiom, without running the statement. A `SCAN` where
you expected a `SEARCH` means the index you were counting on is not being used — `describe` will
tell you whether it exists at all. `analyze` gathers the statistics the planner reads:

```sh
inillucent --db app.rdb analyze
```

## 5. Getting the rows out

```sh
inillucent --db app.rdb export note --format csv --out note.csv
inillucent --db app.rdb export --sql "SELECT id, body FROM note WHERE id < 100" --format json
inillucent --db app.rdb dump                      # the SQL that would rebuild the database
inillucent --db app.rdb backup app-backup.rdb     # a copy of the file
```

`export` takes csv, json, tabs, markdown, insert, quote, line or html. With no `--out` the rows come
back in the result instead of going to a file. With `--out` they are in the file and not in the
result as well, and the result says where it wrote, how many rows went in and how many bytes the
file holds — so `total` against the row count you expected is the check worth making.

## Writing, when you have to

```sh
inillucent --db app.rdb exec  "UPDATE note SET body = ?1 WHERE id = ?2" --params '["edited", 1]'
inillucent --db app.rdb batch "BEGIN; UPDATE note SET body='a' WHERE id=1; DELETE FROM note WHERE id=2; COMMIT"
```

`exec` is one statement and answers with the row count. `batch` is several, **as one transaction** —
either all of them take effect or none do. Use `batch` whenever two statements have to agree.

## Two things that will otherwise surprise you

- **Exit code 3, or the status `unsupported`, means the engine has not built that construct.** It is
  not a syntax error and rewording will not help. `inillucent capabilities` says what is there, and
  every row of it is checked against the running engine by a test.
- **Values are dynamically typed, as in SQLite.** A column's declared type is an *affinity* — a rule
  about what it converts on the way in, not a guarantee about what came out. `--output json` reports
  the storage class each column's values actually had, and `mixed` when they disagreed. That is the
  fact; the declared type in `describe` is the hint.

## Anything the verbs do not cover

```sh
inillucent --db app.rdb run ".schema note
SELECT count(*) FROM note;"
```

`run` takes shell input — dot commands and SQL — and gives you back what it printed. Everything the
interactive shell can do is reachable through it, which matters when you are driving from a script
and there is no terminal.
