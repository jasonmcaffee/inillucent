# A todo service with a REST API, in Rust

`todo-server` is the TodoMVC backend grown into a small team todo service. People own lists, lists
hold todos, and a todo can have subtasks to any depth, tags, comments, a due date and an assignee.
It answers HTTP requests with JSON, and it keeps everything in one inillucent database file.

The program uses the `inillucent` crate from crates.io, exactly as an application outside the
inillucent repository would. It is about 2,700 lines of Rust, and the comments explain each step.
All the SQL is in `src/store/`, so the whole contract between the service and the database is in one
folder.

The point of the example is the SQL. It uses foreign keys that cascade, triggers that write a
history, a view, recursive CTEs, window functions, filtered aggregates, `UPSERT`, `RETURNING`, JSON
functions and FTS5 keyword search, all in the one file.

## Terms used on this page

| Term | Meaning |
|---|---|
| TodoMVC | a well known sample todo application. Its features are add, complete, toggle all, filter by active or completed, and clear completed |
| REST | an HTTP API where each URL names a thing, such as `/todos/12`, and the method says what to do with it |
| CTE | a common table expression: a named query written in `WITH` before the main one. A recursive CTE repeats itself to walk a tree |
| window function | a function such as `rank()` or `row_number()` that looks at other rows of the result without merging them into one row |
| FTS5 | SQLite's full text search table. inillucent has it built in |
| BM25 | the formula FTS5 uses to rank the rows that contain the search words |
| cascade | a foreign key rule: deleting a row deletes, or updates, the rows that point at it |

## 1. Install

You need a Rust toolchain from [rustup.rs](https://rustup.rs). The `inillucent` command line is
optional. It is useful for looking inside the database the service writes:

```sh
npm install -g inillucent          # or brew, pip, cargo, or an install script
```

This example needs no embedding model. It uses the relational engine and FTS5 only.

## 2. Build, fill and start

```sh
cd examples/todo-mvc
cargo build --release
cargo run --release -- seed          # two people, three lists, a dozen todos
cargo run --release -- serve         # http://127.0.0.1:3000
```

The first build compiles inillucent from crates.io and takes a few minutes. The database is
`data/todo.rdb`. `serve` creates it, with its schema, when it does not exist, so `seed` is optional.

| Option | What it does | Default |
|---|---|---|
| `--db` | the database file, for both commands | `data/todo.rdb` |
| `--addr` | the address `serve` listens on. Port `0` picks a free port | `127.0.0.1:3000` |

`serve` prints `listening on http://127.0.0.1:3000` once it is ready.

## 3. Use it

```sh
curl -s -X POST localhost:3000/lists/1/todos -H 'Content-Type: application/json' \
  -d '{"title":"Fix the gate latch","notes":"It sticks in the rain","due_on":"2026-10-03","assignee_id":2,"tags":["outdoor","Repairs"]}'
```

```json
{
  "id": 13,
  "list_id": 1,
  "list_name": "Home",
  "parent_id": null,
  "title": "Fix the gate latch",
  "notes": "It sticks in the rain",
  "completed": false,
  "priority": 2,
  "due_on": "2026-10-03",
  "overdue": false,
  "assignee_id": 2,
  "assignee_name": "Grace",
  "position": 5,
  "tags": ["outdoor", "Repairs"],
  "subtasks": 0,
  "subtasks_done": 0,
  "comments": 0,
  "created_at": "2026-09-25T20:53:29Z",
  "updated_at": "2026-09-25T20:53:29Z",
  "completed_at": null
}
```

Every todo comes back in this form, from every endpoint. It is one row of the `todo_card` view,
which joins the list and the assignee and counts the subtasks and comments.

### Every route

| Method and path | What it does |
|---|---|
| `GET /health` | `{"ok": true, "today": "..."}` once the database is open |
| `GET /people`, `POST /people` | every person with how many todos they have open, overdue and done; add a person |
| `GET /people/{id}`, `DELETE /people/{id}` | one person; delete them, their lists and their todos |
| `GET /people/{id}/workload` | their open todos, ranked in the order to do them |
| `GET /lists`, `POST /lists` | every list with its counts, the busiest first; add a list |
| `GET /lists/{id}`, `PATCH /lists/{id}`, `DELETE /lists/{id}` | one list; rename it; delete it and its todos |
| `GET /lists/{id}/todos`, `POST /lists/{id}/todos` | the todos in a list, filtered and sorted; add one |
| `POST /lists/{id}/toggle-all` | `{"completed": true}` completes every todo in the list, `false` reopens them all |
| `DELETE /lists/{id}/completed` | delete the completed todos, with their subtasks |
| `PUT /lists/{id}/order` | `{"todo_ids": [...]}` puts the top level todos in a new order |
| `GET /todos/{id}` | a todo with its parents, its whole subtask tree and its progress |
| `PATCH /todos/{id}` | change any field, move it to another list, or put it under another todo |
| `DELETE /todos/{id}` | delete it and every subtask below it |
| `PUT /todos/{id}/tags` | `{"tags": [...]}` replaces its tags |
| `GET /todos/{id}/comments`, `POST /todos/{id}/comments` | its comments; add one |
| `GET /todos/{id}/timeline` | everything that happened to it, even after it was deleted |
| `GET /tags` | every tag with how many todos use it |
| `GET /search?q=` | keyword search over titles and notes, across every list |
| `GET /agenda` | what is overdue, and what is due on each of the next 7 days |
| `GET /stats/completions` | todos completed and reopened each day, with a running total |

`GET /lists/{id}/todos` takes `status` (`all`, `active`, `completed`), `tag`, `assignee_id`, `sort`
(`position`, `due`, `priority`) and `subtasks=true`. It answers with the todos and the three counts
the TodoMVC footer shows:

```sh
curl -s 'localhost:3000/lists/1/todos?status=active&sort=due'
```

```json
{
  "todos": [
    { "id": 1, "title": "Paint the fence", "due_on": "2026-09-23", "overdue": true, "tags": ["outdoor", "weekend"], "subtasks": 2, "subtasks_done": 0 },
    { "id": 5, "title": "Buy groceries", "due_on": "2026-09-25", "overdue": false, "tags": ["shopping"] },
    { "id": 6, "title": "File the tax return", "due_on": "2026-09-30", "overdue": false, "tags": ["paperwork"] },
    { "id": 13, "title": "Fix the gate latch", "due_on": "2026-10-03", "overdue": false, "tags": ["outdoor", "Repairs"] },
    { "id": 7, "title": "Call the plumber", "due_on": null, "overdue": false, "tags": [] }
  ],
  "all": 5,
  "active": 5,
  "completed": 0
}
```

The todos above are shortened to a few fields each.

Every endpoint that talks about "overdue" or "due" takes `?today=YYYY-MM-DD`, and the agenda takes
`?from=`. Without them the day is the database's own `date('now')`. The tests pass a fixed day, so
they give the same answer on any day they run.

### Errors

Every error has the same body. The status comes from the engine's own status when the database
refused the request:

| HTTP status | When | Example `message` |
|---|---|---|
| `400` | the body, the query string or the id in the path does not parse | ``unknown field `colour`, expected one of `title`, ...`` |
| `404` | the row does not exist | `todo 77 does not exist` |
| `409` | a constraint in the schema refused the write | `CHECK constraint failed: due_on IS NULL OR due_on IS date(due_on)` |
| `409` | the change would break the subtask tree | `todo 3 is todo 1 or one of its subtasks, so it cannot be its parent` |
| `501` | the engine answered `unsupported`: this build has not implemented a construct the SQL uses | |

```json
{ "error": "conflict", "message": "UNIQUE constraint failed: person.email" }
```

The service does not check a due date, a priority, a blank title or a duplicate email itself. The
schema declares those rules, the engine enforces them with status `constraint`, and `src/error.rs`
turns that status into `409`.

## The schema

```text
person ──< list ──< todo ──< todo_tag >── tag
  │                 │ │
  │                 │ └──< comment >── person (the author)
  └──── assignee ───┘ │
                      └── parent_id: a todo can have subtasks, to any depth

activity   one row per change to a todo, written by triggers
todo_fts   an FTS5 table over each todo's title and notes, written by triggers
todo_card  a view: a todo with its list, assignee, tags and subtask counts
```

`src/schema.rs` has every statement. What each part shows:

| Part | What it shows |
|---|---|
| `REFERENCES ... ON DELETE CASCADE` | deleting a person deletes their lists, their lists' todos, those todos' subtasks, tag links and comments. No Rust code walks any of it |
| `ON DELETE SET NULL` on `assignee_id` | deleting a person unassigns the todos in other people's lists |
| `CHECK (due_on IS NULL OR due_on IS date(due_on))` | `2026-02-30` and `2026-2-3` are refused. `date()` returns NULL for them, and `IS` compares NULL as a value, so the check is false. With `=` the check would be NULL, and a CHECK that is NULL passes |
| `STRICT` tables | a text value in an `INTEGER` column is refused, not stored as text |
| `COLLATE NOCASE` on `person.email` and `tag.name` | `ADA@example.com` is the same person as `ada@example.com`, and `Outdoor` the same tag as `outdoor` |
| `WITHOUT ROWID` on `todo_tag` | the link table is stored in the order of its primary key, `(todo_id, tag_id)`, with no second copy |
| a partial index, `ON todo (due_on) WHERE completed = 0` | only open todos are asked about by due date, so completed ones are left out of the index |
| seven triggers | `created`, `completed`, `reopened`, `renamed`, `moved`, `assigned`, `deleted` and `comment` rows in `activity`. `todo_completed` also stamps and clears `completed_at` |
| three more triggers | keep `todo_fts` in step with `todo`: an insert adds the search entry, a change to the title or notes rewrites it, and a delete, including one an `ON DELETE CASCADE` makes, removes it |
| the `todo_card` view | one definition of "a todo as the API shows it", used by every endpoint |

## The queries

Each of these is in `src/store/`, with a comment that explains it.

### The list overview: join, aggregate, then rank

`GET /lists` joins every list to its top level todos, counts them per list, and ranks the lists with
a window function:

```sql
SELECT l.id, l.name, o.name AS owner_name,
       count(t.id) AS total,
       count(t.id) FILTER (WHERE t.completed = 0) AS open,
       count(t.id) FILTER (WHERE t.completed = 0 AND t.due_on < ?1) AS overdue,
       round(100.0 * count(t.id) FILTER (WHERE t.completed = 1) / count(t.id), 1) AS percent_done,
       min(t.due_on) FILTER (WHERE t.completed = 0) AS next_due,
       rank() OVER (ORDER BY count(t.id) FILTER (WHERE t.completed = 0) DESC) AS busiest_rank
FROM list l
JOIN person o ON o.id = l.owner_id
LEFT JOIN todo t ON t.list_id = l.id AND t.parent_id IS NULL
GROUP BY l.id, l.name, l.owner_id, o.name, l.created_at
```

`count(t.id) FILTER (WHERE ...)` counts only the rows that pass the filter, so one scan gives four
numbers. The `LEFT JOIN` keeps a list with no todos as one row whose todo columns are NULL, and
`count(t.id)` counts NULL as nothing, so that list counts 0. `t.parent_id IS NULL` is in the `ON`
clause because in `WHERE` it would be tested after the join and would drop that row.

`GET /lists/{id}` needs the same rank, which depends on every list, so it ranks them all in a
derived table and picks one row in the outer query: `SELECT * FROM (...) WHERE id = ?2`.

### The subtask tree: a recursive CTE with a sort path

`GET /todos/{id}` returns every todo below one, in the order a tree view draws them:

```sql
WITH RECURSIVE tree (id, parent_id, title, completed, depth, path) AS (
  SELECT id, parent_id, title, completed, 1, printf('%08d.%08d', position, id)
  FROM todo WHERE parent_id = ?1
  UNION ALL
  SELECT c.id, c.parent_id, c.title, c.completed, tree.depth + 1,
         tree.path || '/' || printf('%08d.%08d', c.position, c.id)
  FROM todo c JOIN tree ON c.parent_id = tree.id
)
SELECT id, parent_id, title, completed, depth FROM tree ORDER BY path
```

Sorting by the path puts every todo straight after its parent. A second recursive CTE walks up
instead of down, for the `ancestors` breadcrumb, and a third refuses a `PATCH` that would put a todo
under one of its own subtasks.

### Moving a subtree: an UPDATE that reads a recursive CTE

```sql
WITH RECURSIVE subtree (id) AS (
  SELECT ?1
  UNION ALL
  SELECT t.id FROM todo t JOIN subtree s ON t.parent_id = s.id
)
UPDATE todo SET list_id = ?2 WHERE id IN (SELECT id FROM subtree)
```

The `todo_moved` trigger then writes one `moved` row to `activity` for each todo that moved.

### A person's workload: three window functions over the same rows

```sql
SELECT t.id, t.title, t.priority, t.due_on,
       row_number() OVER (ORDER BY t.priority, t.due_on IS NULL, t.due_on, t.id) AS rank,
       CASE WHEN t.due_on IS NOT NULL
            THEN count(*) OVER (ORDER BY t.due_on IS NULL, t.due_on) END AS due_by_then,
       count(*) OVER (PARTITION BY t.list_id) AS in_same_list
FROM todo t JOIN list l ON l.id = t.list_id
WHERE t.assignee_id = ?1 AND t.completed = 0
ORDER BY rank
```

`row_number()` gives the order to do them in. `count(*) OVER (ORDER BY due)` is a running count: how
many todos are due by this one's day. `PARTITION BY` counts the todos in the same list without a
`GROUP BY` that would merge them into one row.

### Tags: one JSON parameter, `json_each`, and `UPSERT`

A todo's tags travel as one JSON array bound to one parameter. `json_each` turns it into rows inside
the SQL:

```sql
INSERT INTO tag (name) SELECT DISTINCT value FROM json_each(?1) WHERE true
ON CONFLICT (name) DO NOTHING;

INSERT INTO todo_tag (todo_id, tag_id)
SELECT ?1, id FROM tag WHERE name IN (SELECT value FROM json_each(?2));
```

Two statements, however many tags. `WHERE true` is SQLite's rule: without it the parser reads
`ON CONFLICT` as the `ON` of a join.

### The timeline: triggers, and a join on a JSON field

Every change to a todo is a row in `activity`, written by a trigger. A comment's row holds its
author's id inside the JSON detail, and the timeline joins it to `person` with the `->>` operator:

```sql
SELECT a.at, a.kind, a.detail, p.name AS author_name
FROM activity a
LEFT JOIN person p ON p.id = a.detail ->> '$.author_id'
WHERE a.todo_id = ?1
ORDER BY a.id
```

`activity` has no foreign key on purpose, so the history of a deleted todo is still there.

### The agenda: a calendar from a recursive CTE

```sql
WITH RECURSIVE calendar (day) AS (
  SELECT ?1
  UNION ALL
  SELECT date(day, '+1 day') FROM calendar WHERE day < date(?1, printf('+%d days', ?2 - 1))
)
SELECT calendar.day, c.*
FROM calendar
LEFT JOIN (SELECT * FROM todo_card WHERE completed = 0) c ON c.due_on = calendar.day
ORDER BY calendar.day, c.priority
```

A day with nothing due is still in the answer, with no todos. `GET /stats/completions` uses the same
calendar, joined to the `activity` rows grouped by day, with `sum(...) OVER (ORDER BY day)` for the
running total.

### Search: FTS5 joined to the view

```sql
SELECT c.*, bm25(todo_fts, 10.0, 1.0) AS score,
       highlight(todo_fts, 0, '[', ']') AS title_marked,
       snippet(todo_fts, 1, '[', ']', '...', 10) AS notes_snippet
FROM todo_fts
JOIN todo_card c ON c.id = todo_fts.rowid
WHERE todo_fts MATCH ?2
ORDER BY score
```

`bm25(todo_fts, 10.0, 1.0)` counts a match in the title ten times a match in the notes. The search
box text becomes `"paint"* "fen"*`: each word quoted, so `-`, `:` and `*` cannot reach the FTS5 query
language as operators, and each a prefix, so `fen` finds `fence`. The table uses the porter
tokenizer, so `paint` also finds `painting`.

```sh
curl -s 'localhost:3000/search?q=paint%20fen'
```

```json
[
  {
    "id": 1,
    "title": "Paint the fence",
    "score": -7.911720771945685,
    "title_marked": "[Paint] the [fence]",
    "notes_snippet": "Two coats of the green [paint] from the shed"
  }
]
```

The hit above is shortened. Each hit is a whole todo, with the four search fields added.

## Transactions

`SharedDatabase` runs one statement at a time from any number of threads, and a transaction holds
the database until it commits. Every change that touches more than one row is one transaction:

| Change | What the transaction holds |
|---|---|
| create a todo | check the list and the parent, insert the todo, write its tags |
| change a todo | check the move and the new parent, move the subtree, update the fields |
| delete a todo, a list, or the completed todos | count the todos that will go, then delete the rows and let the cascades run |
| reorder a list | check the request names every top level todo once, then write every position with one `UPDATE ... FROM json_each(?1)` |

A `SharedTransaction` that is dropped without `commit` rolls back, so an early return with `?` never
leaves half a change behind.

The triggers in `SEARCH_TRIGGERS` keep `todo_fts` in step inside the same statement, so a search
sees a todo and its entry change together.

## Which inillucent it needs

The example needs inillucent 1.0.31 or later. Building it against 1.0.30 found six problems that
1.0.31 fixes, each checked against SQLite 3.53.4: the overview's `LEFT JOIN` with a second condition
in `ON` returned NULL for every todo column, a trigger writing `todo_fts` made every write to `todo`
fail, a window function inside a derived table was refused, `ORDER BY` inside `json_group_array`
ignored `tag.name`'s collation, `json_group_array(json_object(...))` returned an array of strings, and
`UPDATE ... FROM json_each(...)` changed no rows.

## Tests

```sh
cargo test --release
```

`tests/e2e.rs` starts the built server on a free port, with a new database in a folder of its own,
and calls it over HTTP. Nothing reaches into the server's code, so the tests check what a client
sees. The five tests take about a second once the program is built.

| Test | What it checks |
|---|---|
| `the_todomvc_flow_works_and_survives_a_restart` | add, filter by status, the footer counts, complete and reopen with `completed_at` stamped and cleared, toggle all, clear completed, reorder and the checks on it, rename, delete, and that the data is still there after the server restarts |
| `subtasks_form_a_tree_that_moves_and_deletes_together` | the tree order and depth, progress over the whole tree, the breadcrumb, hiding subtasks by default, refusing a loop or a parent in another list, moving a subtree to another list, and deleting a subtree with its search entries |
| `the_schema_enforces_its_rules` | a duplicate email in another case, a duplicate list name, a missing owner, a blank title, a priority out of range, `2026-02-30`, an unknown assignee, a missing field, an unknown field, a bad id, an unknown route, `null` clearing a field, and the cascades when a person is deleted |
| `tags_comments_and_the_timeline` | tags shared across todos ignoring case, sorted ignoring case, counted by `/tags`, filtered on; and every kind of timeline entry in order, with the details, still there after the todo is deleted |
| `reports_answer_across_lists` | search ranking by title before notes, stemming, prefixes, highlights, operator characters, a list filter and reindexing on edit; the overview counts and ranks on a fixed day; the workload ranks and running counts; the agenda with a free day; and completions per day |

The unit tests in `src/store/reports.rs` check how the search box text becomes an FTS5 query.

## Where the code is

| File | What it does |
|---|---|
| `src/main.rs` | the command line: `serve` and `seed` |
| `src/routes.rs` | every route, and the handler that calls the store |
| `src/error.rs` | one error type, and how an engine status becomes an HTTP status |
| `src/schema.rs` | the tables, indexes, triggers and view |
| `src/store/mod.rs` | opening the database, and reading a row by column name |
| `src/store/people.rs` | people, and the workload |
| `src/store/lists.rs` | lists, the overview, the filtered todos, toggle all, clear completed and reorder |
| `src/store/todos.rs` | one todo: create, read with its tree, update, move, delete, and its search entry |
| `src/store/tags.rs` | tags, comments and the timeline |
| `src/store/reports.rs` | search, the agenda and completions per day |
| `src/seed.rs` | the demo data |

## Looking inside the database

The file is an ordinary inillucent database, and the `inillucent` command line can open it while
the server is running. Use version 1.0.30 or later, which reads the file format the crate writes:

```sh
inillucent --db data/todo.rdb tables
inillucent --db data/todo.rdb query "SELECT kind, count(*) FROM activity GROUP BY kind ORDER BY 2 DESC"
inillucent-shell data/todo.rdb
```

## Licence

The code in this folder is under the MIT licence, like the rest of the repository.
