//! The schema: every table, index, trigger and view, in one statement list.
//!
//! The database is one `.rdb` file. `Store::open` runs this the first time it
//! opens a file with no `todo` table, and never again.
//!
//! ## The tables
//!
//! ```text
//! person ──< list ──< todo ──< todo_tag >── tag
//!   │                 │ │
//!   │                 │ └──< comment >── person (the author)
//!   └──── assignee ───┘ │
//!                       └── parent_id: a todo can have subtasks, to any depth
//!
//! activity   one row per change to a todo, written by triggers
//! todo_fts   an FTS5 table over each todo's title and notes, for /search
//! todo_card  a view: a todo with its list, assignee, tags and subtask counts
//! ```
//!
//! ## What the database enforces, so the Rust code does not have to
//!
//! - **Foreign keys.** `Store::open` turns them on with `PRAGMA foreign_keys =
//!   ON`, which is off by default in inillucent exactly as in SQLite. Deleting
//!   a list deletes its todos, deleting a todo deletes its subtasks, tags links
//!   and comments, and deleting a person unassigns their todos. None of that
//!   is written in Rust.
//! - **CHECK constraints.** A blank title, a priority outside 1 to 3, and a due
//!   date that is not a real `YYYY-MM-DD` day are refused by the engine with
//!   status `constraint`. `due_on IS date(due_on)` works because `date()`
//!   returns NULL for `2026-02-30`, and `IS` compares NULL as a value, so the
//!   check fails. `=` would compare with NULL, give NULL, and a CHECK that
//!   gives NULL passes.
//! - **STRICT tables.** A text value written to an `INTEGER` column is refused
//!   rather than stored as text.
//! - **Case insensitive uniqueness.** `COLLATE NOCASE` on `person.email` and
//!   `tag.name` makes `Home` and `home` the same tag.
//!
//! ## Why the search table is not kept up to date by a trigger
//!
//! The usual SQLite pattern keeps an FTS5 table in step with its source table
//! with three triggers. inillucent 1.0.30 refuses every write to a table
//! that has a trigger writing a virtual table, with status `syntax` and the
//! message "bad parameter or other API misuse". So `todo_fts` is written by
//! the store, in the same transaction as the change to `todo`, and it is just
//! as consistent: a search sees the todo and its index entry change together.
//! See `store/todos.rs`, `index_todo`.

/// The schema. Run once, when the database has no `todo` table.
pub const SCHEMA: &str = "
CREATE TABLE person (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL CHECK (length(trim(name)) > 0),
  email       TEXT NOT NULL UNIQUE COLLATE NOCASE,
  created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
) STRICT;

CREATE TABLE list (
  id          INTEGER PRIMARY KEY,
  owner_id    INTEGER NOT NULL REFERENCES person (id) ON DELETE CASCADE,
  name        TEXT NOT NULL CHECK (length(trim(name)) > 0),
  created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  UNIQUE (owner_id, name)
) STRICT;

CREATE TABLE todo (
  id            INTEGER PRIMARY KEY,
  list_id       INTEGER NOT NULL REFERENCES list (id) ON DELETE CASCADE,
  parent_id     INTEGER REFERENCES todo (id) ON DELETE CASCADE,
  title         TEXT NOT NULL CHECK (length(trim(title)) > 0),
  notes         TEXT NOT NULL DEFAULT '',
  completed     INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
  priority      INTEGER NOT NULL DEFAULT 2 CHECK (priority BETWEEN 1 AND 3),
  due_on        TEXT CHECK (due_on IS NULL OR due_on IS date(due_on)),
  assignee_id   INTEGER REFERENCES person (id) ON DELETE SET NULL,
  position      INTEGER NOT NULL DEFAULT 0,
  created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  completed_at  TEXT
) STRICT;

-- A list's todos in the order the user put them.
CREATE INDEX todo_list_position ON todo (list_id, position);
-- A todo's subtasks, for the recursive walks and the ON DELETE CASCADE.
CREATE INDEX todo_parent ON todo (parent_id);
-- Who is working on what.
CREATE INDEX todo_assignee ON todo (assignee_id);
-- A partial index: only open todos are ever asked about by due date, so
-- completed ones are left out of it and it stays small.
CREATE INDEX todo_open_due ON todo (due_on) WHERE completed = 0;

CREATE TABLE tag (
  id    INTEGER PRIMARY KEY,
  name  TEXT NOT NULL UNIQUE COLLATE NOCASE CHECK (length(trim(name)) > 0)
) STRICT;

-- The many to many link between todos and tags. The primary key is the pair,
-- so a todo cannot have the same tag twice, and WITHOUT ROWID stores the rows
-- in that key's order with no second copy.
CREATE TABLE todo_tag (
  todo_id  INTEGER NOT NULL REFERENCES todo (id) ON DELETE CASCADE,
  tag_id   INTEGER NOT NULL REFERENCES tag (id) ON DELETE CASCADE,
  PRIMARY KEY (todo_id, tag_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX todo_tag_by_tag ON todo_tag (tag_id);

CREATE TABLE comment (
  id          INTEGER PRIMARY KEY,
  todo_id     INTEGER NOT NULL REFERENCES todo (id) ON DELETE CASCADE,
  author_id   INTEGER REFERENCES person (id) ON DELETE SET NULL,
  body        TEXT NOT NULL CHECK (length(trim(body)) > 0),
  created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
) STRICT;
CREATE INDEX comment_todo ON comment (todo_id);

-- The history of every todo. It has no foreign key on purpose: the row that
-- says a todo was deleted has to outlive the todo.
CREATE TABLE activity (
  id       INTEGER PRIMARY KEY,
  todo_id  INTEGER NOT NULL,
  list_id  INTEGER NOT NULL,
  kind     TEXT NOT NULL,
  detail   TEXT NOT NULL DEFAULT '{}',
  at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
) STRICT;
CREATE INDEX activity_todo ON activity (todo_id);

-- Keyword search over titles and notes. The porter tokenizer matches
-- `painting` to `paint`.
CREATE VIRTUAL TABLE todo_fts USING fts5(title, notes, tokenize = 'porter unicode61');

-- The triggers below write the activity log. They fire for every row a
-- statement changes, including the rows an ON DELETE CASCADE removes, so
-- clearing a list records one `deleted` row per todo without any Rust code.

CREATE TRIGGER todo_created AFTER INSERT ON todo BEGIN
  INSERT INTO activity (todo_id, list_id, kind, detail)
  VALUES (NEW.id, NEW.list_id, 'created', json_object('title', NEW.title));
END;

-- Completing a todo stamps completed_at, and reopening it clears the stamp.
-- The WHEN clause means setting completed to the value it already has records
-- nothing.
CREATE TRIGGER todo_completed AFTER UPDATE OF completed ON todo
WHEN NEW.completed <> OLD.completed BEGIN
  UPDATE todo
  SET completed_at = CASE WHEN NEW.completed = 1 THEN strftime('%Y-%m-%dT%H:%M:%SZ', 'now') END
  WHERE id = NEW.id;
  INSERT INTO activity (todo_id, list_id, kind)
  VALUES (NEW.id, NEW.list_id, CASE WHEN NEW.completed = 1 THEN 'completed' ELSE 'reopened' END);
END;

CREATE TRIGGER todo_renamed AFTER UPDATE OF title ON todo
WHEN NEW.title <> OLD.title BEGIN
  INSERT INTO activity (todo_id, list_id, kind, detail)
  VALUES (NEW.id, NEW.list_id, 'renamed', json_object('from', OLD.title, 'to', NEW.title));
END;

CREATE TRIGGER todo_moved AFTER UPDATE OF list_id ON todo
WHEN NEW.list_id <> OLD.list_id BEGIN
  INSERT INTO activity (todo_id, list_id, kind, detail)
  VALUES (NEW.id, NEW.list_id, 'moved', json_object('from_list', OLD.list_id, 'to_list', NEW.list_id));
END;

CREATE TRIGGER todo_assigned AFTER UPDATE OF assignee_id ON todo
WHEN NEW.assignee_id IS NOT OLD.assignee_id BEGIN
  INSERT INTO activity (todo_id, list_id, kind, detail)
  VALUES (NEW.id, NEW.list_id, 'assigned', json_object('from', OLD.assignee_id, 'to', NEW.assignee_id));
END;

CREATE TRIGGER todo_deleted AFTER DELETE ON todo BEGIN
  INSERT INTO activity (todo_id, list_id, kind, detail)
  VALUES (OLD.id, OLD.list_id, 'deleted', json_object('title', OLD.title));
END;

-- A comment goes into the history too, so a todo's timeline is one table read
-- in id order. The author is stored as an id inside the JSON detail, and the
-- timeline query joins it to person with the ->> operator.
CREATE TRIGGER comment_added AFTER INSERT ON comment BEGIN
  INSERT INTO activity (todo_id, list_id, kind, detail)
  SELECT NEW.todo_id, t.list_id, 'comment',
         json_object('comment_id', NEW.id, 'author_id', NEW.author_id, 'body', NEW.body)
  FROM todo t WHERE t.id = NEW.todo_id;
END;

-- One todo as every endpoint shows it: its list's name, its assignee's name,
-- its tags as a JSON array in name order, and how many subtasks and comments
-- it has. Each count is a correlated subquery, which the engine runs once per
-- todo it returns.
--
-- The tags are sorted by lower(g.name). ORDER BY g.name should be enough,
-- because tag.name is COLLATE NOCASE, but inillucent 1.0.30 ignores the
-- collation in an ORDER BY inside an aggregate and puts 'Repairs' before
-- 'outdoor'.
CREATE VIEW todo_card AS
SELECT
  t.id, t.list_id, l.name AS list_name, t.parent_id, t.title, t.notes, t.completed,
  t.priority, t.due_on, t.assignee_id, a.name AS assignee_name, t.position,
  t.created_at, t.updated_at, t.completed_at,
  (SELECT json_group_array(g.name ORDER BY lower(g.name))
   FROM todo_tag tt JOIN tag g ON g.id = tt.tag_id
   WHERE tt.todo_id = t.id) AS tags,
  (SELECT count(*) FROM todo s WHERE s.parent_id = t.id) AS subtasks,
  (SELECT count(*) FROM todo s WHERE s.parent_id = t.id AND s.completed = 1) AS subtasks_done,
  (SELECT count(*) FROM comment c WHERE c.todo_id = t.id) AS comments
FROM todo t
JOIN list l ON l.id = t.list_id
LEFT JOIN person a ON a.id = t.assignee_id;
";
