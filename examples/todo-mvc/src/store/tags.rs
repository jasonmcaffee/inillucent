//! Tags, comments, and the timeline of everything that happened to a todo.

use inillucent::{SharedTransaction, Value};
use serde::{Deserialize, Serialize};

use super::{optional_int, text, tx_execute, tx_query, Record, Store};
use crate::error::{ApiError, ApiResult};

/// A tag and how many todos use it.
#[derive(Debug, Serialize)]
pub struct TagUse {
    /// The tag's id.
    pub id: i64,
    /// Its name, in the case it was first written in.
    pub name: String,
    /// How many todos have it.
    pub todos: i64,
    /// How many of those are not done.
    pub open: i64,
}

/// The body of `POST /todos/{id}/comments`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewComment {
    /// Who wrote it.
    #[serde(default)]
    pub author_id: Option<i64>,
    /// What they wrote.
    pub body: String,
}

/// A comment on a todo.
#[derive(Debug, Serialize)]
pub struct Comment {
    /// The comment's id.
    pub id: i64,
    /// The todo it is on.
    pub todo_id: i64,
    /// Who wrote it.
    pub author_id: Option<i64>,
    /// Their name. Nothing once they are deleted.
    pub author_name: Option<String>,
    /// What they wrote.
    pub body: String,
    /// When.
    pub created_at: String,
}

/// One thing that happened to a todo: a change the triggers recorded, or a comment.
#[derive(Debug, Serialize)]
pub struct TimelineEntry {
    /// When it happened.
    pub at: String,
    /// `created`, `completed`, `reopened`, `renamed`, `moved`, `assigned`,
    /// `tagged`, `deleted` or `comment`.
    pub kind: String,
    /// What changed, such as `{"from": "Old title", "to": "New title"}`.
    pub detail: serde_json::Value,
    /// Who wrote it, for a comment.
    pub author_name: Option<String>,
}

impl Store {
    /// Replaces a todo's tags, and records the new set in its timeline.
    ///
    /// @param todo_id - the todo
    /// @param tags - the tag names; an empty list removes every tag
    pub fn set_tags(&self, todo_id: i64, tags: &[String]) -> ApiResult<Vec<String>> {
        let tx = self.begin()?;
        let found = tx_query(&tx, "SELECT 1 AS found FROM todo WHERE id = ?1", &[Value::Integer(todo_id)])?;
        if Record::first(&found).is_none() {
            return Err(ApiError::not_found(format!("todo {todo_id}")));
        }
        replace_tags(&tx, todo_id, tags)?;
        tx_execute(
            &tx,
            "INSERT INTO activity (todo_id, list_id, kind, detail)
             SELECT id, list_id, 'tagged', json_object('tags', json(?2)) FROM todo WHERE id = ?1",
            &[Value::Integer(todo_id), text(&tags_json(tags))],
        )?;
        tx_execute(&tx, "UPDATE todo SET updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?1", &[Value::Integer(todo_id)])?;
        tx.commit()?;
        Ok(self.todo_card(todo_id, None)?.tags)
    }

    /// Returns every tag with how many todos use it, most used first.
    ///
    /// Two `LEFT JOIN`s keep a tag that no todo uses: it comes back with
    /// zeros instead of being left out.
    pub fn tags(&self) -> ApiResult<Vec<TagUse>> {
        let rows = self.query(
            "SELECT g.id, g.name, count(tt.todo_id) AS todos, count(t.id) FILTER (WHERE t.completed = 0) AS open
             FROM tag g
             LEFT JOIN todo_tag tt ON tt.tag_id = g.id
             LEFT JOIN todo t ON t.id = tt.todo_id
             GROUP BY g.id
             ORDER BY todos DESC, g.name",
            &[],
        )?;
        Ok(Record::all(&rows)
            .map(|row| TagUse { id: row.int("id"), name: row.text("name"), todos: row.int("todos"), open: row.int("open") })
            .collect())
    }

    /// Adds a comment to a todo.
    ///
    /// A missing todo or author is refused by the foreign keys, which answer
    /// with status `constraint` and so `409`.
    ///
    /// @param todo_id - the todo
    /// @param comment - the author and the text
    pub fn add_comment(&self, todo_id: i64, comment: &NewComment) -> ApiResult<Comment> {
        let rows = self.query(
            "INSERT INTO comment (todo_id, author_id, body) VALUES (?1, ?2, ?3) RETURNING id",
            &[Value::Integer(todo_id), optional_int(comment.author_id), text(comment.body.trim())],
        )?;
        let id = Record::first(&rows).map(|row| row.int("id")).ok_or_else(|| ApiError::conflict("the comment was not added"))?;
        let rows = self.query(&format!("{COMMENT_SELECT} WHERE c.id = ?1"), &[Value::Integer(id)])?;
        Record::first(&rows).map(|row| comment_from(&row)).ok_or_else(|| ApiError::not_found(format!("comment {id}")))
    }

    /// Returns a todo's comments, oldest first.
    ///
    /// @param todo_id - the todo
    pub fn comments(&self, todo_id: i64) -> ApiResult<Vec<Comment>> {
        self.todo_card(todo_id, None)?;
        let rows = self.query(&format!("{COMMENT_SELECT} WHERE c.todo_id = ?1 ORDER BY c.id"), &[Value::Integer(todo_id)])?;
        Ok(Record::all(&rows).map(|row| comment_from(&row)).collect())
    }

    /// Returns everything that happened to a todo, oldest first.
    ///
    /// Every row comes from `activity`, which the triggers write: one row per
    /// change to the todo, and one per comment. The rows are read in id order,
    /// which is the order they were written in. Timestamps would not do, since
    /// several changes happen in the same second.
    ///
    /// A comment's row holds its author's id inside the JSON `detail`.
    /// `a.detail ->> '$.author_id'` reads it out as an integer, and the `LEFT
    /// JOIN` on it finds the author's name, or nothing for any other row.
    ///
    /// The history outlives the todo, because `activity` has no foreign key,
    /// so this also answers for a todo that has been deleted.
    ///
    /// @param todo_id - the todo
    pub fn timeline(&self, todo_id: i64) -> ApiResult<Vec<TimelineEntry>> {
        let rows = self.query(
            "SELECT a.at, a.kind, a.detail, p.name AS author_name
             FROM activity a
             LEFT JOIN person p ON p.id = a.detail ->> '$.author_id'
             WHERE a.todo_id = ?1
             ORDER BY a.id",
            &[Value::Integer(todo_id)],
        )?;
        let entries: Vec<TimelineEntry> = Record::all(&rows)
            .map(|row| TimelineEntry {
                at: row.text("at"),
                kind: row.text("kind"),
                detail: row.json("detail"),
                author_name: row.opt_text("author_name"),
            })
            .collect();
        if entries.is_empty() {
            return Err(ApiError::not_found(format!("todo {todo_id}")));
        }
        Ok(entries)
    }
}

/// Replaces a todo's tags inside the caller's transaction.
///
/// The names travel as one JSON array bound to a single parameter, and
/// `json_each` turns it back into rows inside the SQL, so there is one
/// statement per step however many tags there are:
///
/// 1. delete the todo's links;
/// 2. `INSERT ... ON CONFLICT DO NOTHING` creates the tags that do not exist
///    yet. `tag.name` is `COLLATE NOCASE`, so `Urgent` finds `urgent`;
/// 3. `INSERT ... SELECT` links the todo to every tag whose name is in the
///    array, compared with the same collation.
///
/// `WHERE true` in step 2 is SQLite's rule, not a mistake: without it the
/// parser reads `ON CONFLICT` as the `ON` clause of a join.
///
/// @param tx - the open transaction
/// @param todo_id - the todo
/// @param tags - the tag names
pub(super) fn replace_tags(tx: &SharedTransaction, todo_id: i64, tags: &[String]) -> ApiResult<()> {
    let names = text(&tags_json(tags));
    tx_execute(tx, "DELETE FROM todo_tag WHERE todo_id = ?1", &[Value::Integer(todo_id)])?;
    if tags.is_empty() {
        return Ok(());
    }
    tx_execute(
        tx,
        "INSERT INTO tag (name) SELECT DISTINCT value FROM json_each(?1) WHERE true ON CONFLICT (name) DO NOTHING",
        std::slice::from_ref(&names),
    )?;
    tx_execute(
        tx,
        "INSERT INTO todo_tag (todo_id, tag_id) SELECT ?1, id FROM tag WHERE name IN (SELECT value FROM json_each(?2))",
        &[Value::Integer(todo_id), names],
    )?;
    Ok(())
}

/// Returns the tag names as a JSON array, trimmed, with blank names left out.
///
/// @param tags - the tag names
fn tags_json(tags: &[String]) -> String {
    let names: Vec<&str> = tags.iter().map(|tag| tag.trim()).filter(|tag| !tag.is_empty()).collect();
    serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_string())
}

/// A comment with its author's name. `LEFT JOIN`, because the author may have been deleted.
const COMMENT_SELECT: &str = "
SELECT c.id, c.todo_id, c.author_id, p.name AS author_name, c.body, c.created_at
FROM comment c LEFT JOIN person p ON p.id = c.author_id";

/// Builds a [`Comment`] from a row of `COMMENT_SELECT`.
///
/// @param row - the row
fn comment_from(row: &Record) -> Comment {
    Comment {
        id: row.int("id"),
        todo_id: row.int("todo_id"),
        author_id: row.opt_int("author_id"),
        author_name: row.opt_text("author_name"),
        body: row.text("body"),
        created_at: row.text("created_at"),
    }
}
