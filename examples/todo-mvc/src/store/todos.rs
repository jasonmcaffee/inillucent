//! One todo: create it, read it with its subtasks, change it, move it, delete it.
//!
//! A todo can have subtasks, and a subtask can have its own, to any depth.
//! `todo.parent_id` points at the todo above. Two recursive CTEs walk that
//! chain: one down, for the subtask tree, and one up, for the breadcrumb of
//! parents. Moving a todo to another list moves its whole subtree with one
//! `UPDATE` whose `WHERE` reads a recursive CTE.

use inillucent::{SharedTransaction, Value};
use serde::{Deserialize, Deserializer, Serialize};

use super::{optional_int, optional_text, text, tx_execute, tx_query, Record, Store};
use crate::error::{ApiError, ApiResult};

/// The body of `POST /lists/{id}/todos`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTodo {
    /// What to do.
    pub title: String,
    /// Longer text. Searched along with the title.
    #[serde(default)]
    pub notes: Option<String>,
    /// 1 is high, 3 is low. 2 when left out.
    #[serde(default)]
    pub priority: Option<i64>,
    /// A `YYYY-MM-DD` day.
    #[serde(default)]
    pub due_on: Option<String>,
    /// The person to do it.
    #[serde(default)]
    pub assignee_id: Option<i64>,
    /// The todo this one is a subtask of. It must be in the same list.
    #[serde(default)]
    pub parent_id: Option<i64>,
    /// Tag names. A tag that does not exist yet is created.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// The body of `PATCH /todos/{id}`. Every field is optional, and only the
/// fields present are changed.
///
/// `due_on`, `assignee_id` and `parent_id` can be cleared by sending `null`,
/// so each is an `Option<Option<_>>`: absent is `None`, `null` is
/// `Some(None)`, and a value is `Some(Some(value))`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoPatch {
    /// A new title.
    #[serde(default)]
    pub title: Option<String>,
    /// New notes.
    #[serde(default)]
    pub notes: Option<String>,
    /// Done or not done.
    #[serde(default)]
    pub completed: Option<bool>,
    /// A new priority.
    #[serde(default)]
    pub priority: Option<i64>,
    /// A new due date, or `null` for none.
    #[serde(default, deserialize_with = "nullable")]
    pub due_on: Option<Option<String>>,
    /// A new assignee, or `null` for nobody.
    #[serde(default, deserialize_with = "nullable")]
    pub assignee_id: Option<Option<i64>>,
    /// Another list. The todo's subtasks move with it.
    #[serde(default)]
    pub list_id: Option<i64>,
    /// A new parent, or `null` to make it a top level todo.
    #[serde(default, deserialize_with = "nullable")]
    pub parent_id: Option<Option<i64>>,
}

/// A todo as every endpoint returns it: one row of the `todo_card` view.
#[derive(Debug, Serialize)]
pub struct TodoCard {
    /// The todo's id.
    pub id: i64,
    /// The list it is in.
    pub list_id: i64,
    /// That list's name.
    pub list_name: String,
    /// The todo it is a subtask of.
    pub parent_id: Option<i64>,
    /// What to do.
    pub title: String,
    /// Longer text.
    pub notes: String,
    /// Whether it is done.
    pub completed: bool,
    /// 1 is high, 3 is low.
    pub priority: i64,
    /// The due date.
    pub due_on: Option<String>,
    /// Whether it is not done and its due date has passed.
    pub overdue: bool,
    /// The person doing it.
    pub assignee_id: Option<i64>,
    /// Their name.
    pub assignee_name: Option<String>,
    /// Its place among its siblings, from 1.
    pub position: i64,
    /// Its tags, by name.
    pub tags: Vec<String>,
    /// How many direct subtasks it has.
    pub subtasks: i64,
    /// How many of those are done.
    pub subtasks_done: i64,
    /// How many comments it has.
    pub comments: i64,
    /// When it was created.
    pub created_at: String,
    /// When it was last changed.
    pub updated_at: String,
    /// When it was completed. Set and cleared by the `todo_completed` trigger.
    pub completed_at: Option<String>,
}

/// One todo in a subtask tree.
#[derive(Debug, Serialize)]
pub struct SubtaskNode {
    /// The todo's id.
    pub id: i64,
    /// The todo it is directly under.
    pub parent_id: i64,
    /// What to do.
    pub title: String,
    /// Whether it is done.
    pub completed: bool,
    /// 1 for a direct subtask, 2 for a subtask of a subtask, and so on.
    pub depth: i64,
}

/// A todo with everything around it.
#[derive(Debug, Serialize)]
pub struct TodoDetail {
    /// The todo itself.
    #[serde(flatten)]
    pub card: TodoCard,
    /// The todos above it, from the top level down to its direct parent.
    pub ancestors: Vec<Ancestor>,
    /// Every todo below it, at every depth, in the order a tree view draws them.
    pub subtask_tree: Vec<SubtaskNode>,
    /// How much of the whole tree below it is done.
    pub progress: Progress,
}

/// One todo in the breadcrumb above another.
#[derive(Debug, Serialize)]
pub struct Ancestor {
    /// The todo's id.
    pub id: i64,
    /// Its title.
    pub title: String,
}

/// How many todos below one are done.
#[derive(Debug, Serialize)]
pub struct Progress {
    /// Every subtask at every depth.
    pub total: i64,
    /// The ones that are done.
    pub done: i64,
}

impl Store {
    /// Creates a todo, with its tags and its search entry, in one transaction.
    ///
    /// The new todo goes to the end of its siblings: its position is one more
    /// than the largest among the todos with the same list and the same
    /// parent, read by the `INSERT ... SELECT` itself. `parent_id IS ?2`
    /// compares NULL as a value, so it matches the top level todos when there
    /// is no parent.
    ///
    /// @param list_id - the list to put it in
    /// @param todo - the todo
    pub fn create_todo(&self, list_id: i64, todo: &NewTodo) -> ApiResult<TodoCard> {
        let tx = self.begin()?;
        require_list(&tx, list_id)?;
        if let Some(parent) = todo.parent_id {
            require_same_list(&tx, parent, list_id)?;
        }
        let rows = tx_query(
            &tx,
            "INSERT INTO todo (list_id, parent_id, title, notes, priority, due_on, assignee_id, position)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, coalesce(max(position), 0) + 1
             FROM todo WHERE list_id = ?1 AND parent_id IS ?2
             RETURNING id",
            &[
                Value::Integer(list_id),
                optional_int(todo.parent_id),
                text(todo.title.trim()),
                text(todo.notes.as_deref().unwrap_or("")),
                Value::Integer(todo.priority.unwrap_or(2)),
                optional_text(todo.due_on.as_deref()),
                optional_int(todo.assignee_id),
            ],
        )?;
        let id = Record::first(&rows).map(|row| row.int("id")).ok_or_else(|| ApiError::conflict("the todo was not added"))?;
        super::tags::replace_tags(&tx, id, &todo.tags)?;
        index_todo(&tx, id)?;
        tx.commit()?;
        self.todo_card(id, None)
    }

    /// Returns one todo as a card.
    ///
    /// @param id - the todo's id
    /// @param today - the day that decides what is overdue
    pub fn todo_card(&self, id: i64, today: Option<&str>) -> ApiResult<TodoCard> {
        let today = self.today(today)?;
        let rows = self.query(&format!("{CARD_SELECT} WHERE id = ?2"), &[text(&today), Value::Integer(id)])?;
        Record::first(&rows).map(|row| card_from(&row)).ok_or_else(|| ApiError::not_found(format!("todo {id}")))
    }

    /// Returns a todo with its parents, its whole subtask tree, and its progress.
    ///
    /// @param id - the todo's id
    /// @param today - the day that decides what is overdue
    pub fn todo_detail(&self, id: i64, today: Option<&str>) -> ApiResult<TodoDetail> {
        let card = self.todo_card(id, today)?;
        let ancestors = self.ancestors(id)?;
        let subtask_tree = self.subtask_tree(id)?;
        let done = subtask_tree.iter().filter(|node| node.completed).count() as i64;
        let progress = Progress { total: subtask_tree.len() as i64, done };
        Ok(TodoDetail { card, ancestors, subtask_tree, progress })
    }

    /// Returns the todos above one, from the top level down to its parent.
    ///
    /// The recursive CTE starts at the todo's parent and follows `parent_id`
    /// upwards, one row per step, until it reaches a todo with no parent.
    ///
    /// @param id - the todo's id
    fn ancestors(&self, id: i64) -> ApiResult<Vec<Ancestor>> {
        let rows = self.query(
            "WITH RECURSIVE up (id, parent_id, title, depth) AS (
               SELECT id, parent_id, title, 1 FROM todo WHERE id = (SELECT parent_id FROM todo WHERE id = ?1)
               UNION ALL
               SELECT t.id, t.parent_id, t.title, up.depth + 1 FROM todo t JOIN up ON t.id = up.parent_id
             )
             SELECT id, title FROM up ORDER BY depth DESC",
            &[Value::Integer(id)],
        )?;
        Ok(Record::all(&rows).map(|row| Ancestor { id: row.int("id"), title: row.text("title") }).collect())
    }

    /// Returns every todo below one, in the order a tree view draws them.
    ///
    /// The recursive CTE starts with the direct subtasks and joins each step's
    /// rows to their own subtasks. Each row carries a `path` built from the
    /// positions on the way down, such as `00000002.00000007/00000001.00000009`,
    /// and sorting by that text puts every todo straight after its parent and
    /// before its parent's next sibling. The id after each position keeps two
    /// siblings with the same position in a fixed order.
    ///
    /// @param id - the todo at the top of the tree
    fn subtask_tree(&self, id: i64) -> ApiResult<Vec<SubtaskNode>> {
        let rows = self.query(
            "WITH RECURSIVE tree (id, parent_id, title, completed, depth, path) AS (
               SELECT id, parent_id, title, completed, 1, printf('%08d.%08d', position, id)
               FROM todo WHERE parent_id = ?1
               UNION ALL
               SELECT c.id, c.parent_id, c.title, c.completed, tree.depth + 1,
                      tree.path || '/' || printf('%08d.%08d', c.position, c.id)
               FROM todo c JOIN tree ON c.parent_id = tree.id
             )
             SELECT id, parent_id, title, completed, depth FROM tree ORDER BY path",
            &[Value::Integer(id)],
        )?;
        Ok(Record::all(&rows)
            .map(|row| SubtaskNode {
                id: row.int("id"),
                parent_id: row.int("parent_id"),
                title: row.text("title"),
                completed: row.bool("completed"),
                depth: row.int("depth"),
            })
            .collect())
    }

    /// Changes the fields a `PATCH` names, in one transaction.
    ///
    /// Moving to another list and changing the parent are checked first,
    /// because each can break the tree: a subtask has to be in its parent's
    /// list, and a todo cannot be put under itself or under one of its own
    /// subtasks. The other fields go into one `UPDATE` with only the columns
    /// that were sent. The triggers record what changed in `activity`.
    ///
    /// @param id - the todo's id
    /// @param patch - the fields to change
    pub fn update_todo(&self, id: i64, patch: &TodoPatch) -> ApiResult<TodoCard> {
        let tx = self.begin()?;
        let current = require_todo(&tx, id)?;
        let list_id = patch.list_id.unwrap_or(current.list_id);
        if patch.list_id.is_some_and(|target| target != current.list_id) {
            require_list(&tx, list_id)?;
            move_subtree(&tx, id, list_id)?;
        }
        let parent = match patch.parent_id {
            Some(parent) => parent,
            None if list_id != current.list_id => None,
            None => current.parent_id,
        };
        if parent != current.parent_id || list_id != current.list_id {
            if let Some(parent) = parent {
                require_same_list(&tx, parent, list_id)?;
                refuse_cycle(&tx, id, parent)?;
            }
            reparent(&tx, id, list_id, parent)?;
        }
        update_fields(&tx, id, patch)?;
        if patch.title.is_some() || patch.notes.is_some() {
            index_todo(&tx, id)?;
        }
        tx.commit()?;
        self.todo_card(id, None)
    }

    /// Deletes a todo and every subtask below it, and returns how many todos went.
    ///
    /// `ON DELETE CASCADE` on `parent_id` deletes the subtree, and on
    /// `todo_tag` and `comment` it deletes their rows. The search entries are
    /// deleted by hand, first, because a virtual table has no foreign keys.
    ///
    /// @param id - the todo's id
    pub fn delete_todo(&self, id: i64) -> ApiResult<u64> {
        let tx = self.begin()?;
        require_todo(&tx, id)?;
        let subtree = subtree_ids(&tx, id)?;
        for todo in &subtree {
            tx_execute(&tx, "DELETE FROM todo_fts WHERE rowid = ?1", &[Value::Integer(*todo)])?;
        }
        tx_execute(&tx, "DELETE FROM todo WHERE id = ?1", &[Value::Integer(id)])?;
        tx.commit()?;
        Ok(subtree.len() as u64)
    }
}

/// The `todo_card` view's columns, plus whether the todo is overdue on the day bound to `?1`.
pub(super) const CARD_SELECT: &str = "SELECT *, (completed = 0 AND due_on < ?1) AS overdue FROM todo_card";

/// Builds a [`TodoCard`] from a row of `CARD_SELECT`.
///
/// @param row - the row
pub(super) fn card_from(row: &Record) -> TodoCard {
    TodoCard {
        id: row.int("id"),
        list_id: row.int("list_id"),
        list_name: row.text("list_name"),
        parent_id: row.opt_int("parent_id"),
        title: row.text("title"),
        notes: row.text("notes"),
        completed: row.bool("completed"),
        priority: row.int("priority"),
        due_on: row.opt_text("due_on"),
        overdue: row.bool("overdue"),
        assignee_id: row.opt_int("assignee_id"),
        assignee_name: row.opt_text("assignee_name"),
        position: row.int("position"),
        tags: row.strings("tags"),
        subtasks: row.int("subtasks"),
        subtasks_done: row.int("subtasks_done"),
        comments: row.int("comments"),
        created_at: row.text("created_at"),
        updated_at: row.text("updated_at"),
        completed_at: row.opt_text("completed_at"),
    }
}

/// Writes a todo's search entry again from its current title and notes.
///
/// The old entry is deleted and a new one is copied from the `todo` row with
/// `INSERT ... SELECT`. It runs inside the caller's transaction, so the todo
/// and its entry change together. See `schema.rs` for why a trigger does not
/// do this.
///
/// @param tx - the open transaction
/// @param id - the todo's id
pub(super) fn index_todo(tx: &SharedTransaction, id: i64) -> ApiResult<()> {
    tx_execute(tx, "DELETE FROM todo_fts WHERE rowid = ?1", &[Value::Integer(id)])?;
    tx_execute(tx, "INSERT INTO todo_fts (rowid, title, notes) SELECT id, title, notes FROM todo WHERE id = ?1", &[Value::Integer(id)])?;
    Ok(())
}

/// The list and parent of a todo, read before it is changed.
struct Placement {
    list_id: i64,
    parent_id: Option<i64>,
}

/// Returns a todo's list and parent, or `404` when there is no such todo.
///
/// @param tx - the open transaction
/// @param id - the todo's id
fn require_todo(tx: &SharedTransaction, id: i64) -> ApiResult<Placement> {
    let rows = tx_query(tx, "SELECT list_id, parent_id FROM todo WHERE id = ?1", &[Value::Integer(id)])?;
    Record::first(&rows)
        .map(|row| Placement { list_id: row.int("list_id"), parent_id: row.opt_int("parent_id") })
        .ok_or_else(|| ApiError::not_found(format!("todo {id}")))
}

/// Answers `404` when a list does not exist.
///
/// The foreign key would refuse the write anyway, but with status
/// `constraint`, which the service answers with `409`. A missing list in the
/// URL is a `404`.
///
/// @param tx - the open transaction
/// @param list_id - the list's id
pub(super) fn require_list(tx: &SharedTransaction, list_id: i64) -> ApiResult<()> {
    let rows = tx_query(tx, "SELECT 1 AS found FROM list WHERE id = ?1", &[Value::Integer(list_id)])?;
    match Record::first(&rows) {
        Some(_) => Ok(()),
        None => Err(ApiError::not_found(format!("list {list_id}"))),
    }
}

/// Refuses a parent that is missing or in another list.
///
/// @param tx - the open transaction
/// @param parent - the proposed parent's id
/// @param list_id - the list the child will be in
fn require_same_list(tx: &SharedTransaction, parent: i64, list_id: i64) -> ApiResult<()> {
    let placement = require_todo(tx, parent)?;
    if placement.list_id != list_id {
        return Err(ApiError::conflict(format!(
            "todo {parent} is in list {}, and a subtask must be in its parent's list",
            placement.list_id
        )));
    }
    Ok(())
}

/// Refuses to put a todo under itself or under one of its own subtasks.
///
/// The recursive CTE walks up from the proposed parent. If the walk passes
/// through the todo being moved, the move would make a loop.
///
/// @param tx - the open transaction
/// @param id - the todo being moved
/// @param parent - the proposed parent
fn refuse_cycle(tx: &SharedTransaction, id: i64, parent: i64) -> ApiResult<()> {
    let rows = tx_query(
        tx,
        "WITH RECURSIVE up (id) AS (
           SELECT ?2
           UNION ALL
           SELECT t.parent_id FROM todo t JOIN up ON t.id = up.id WHERE t.parent_id IS NOT NULL
         )
         SELECT count(*) AS loops FROM up WHERE id = ?1",
        &[Value::Integer(id), Value::Integer(parent)],
    )?;
    match Record::first(&rows).map(|row| row.int("loops")) {
        Some(0) => Ok(()),
        _ => Err(ApiError::conflict(format!("todo {parent} is todo {id} or one of its subtasks, so it cannot be its parent"))),
    }
}

/// Moves a todo and every subtask below it to another list.
///
/// One `UPDATE`, whose `WHERE` reads a recursive CTE for the ids in the
/// subtree. The `todo_moved` trigger writes one `activity` row per todo moved.
///
/// @param tx - the open transaction
/// @param id - the todo at the top of the subtree
/// @param list_id - the list to move it to
fn move_subtree(tx: &SharedTransaction, id: i64, list_id: i64) -> ApiResult<()> {
    tx_execute(
        tx,
        "WITH RECURSIVE subtree (id) AS (
           SELECT ?1
           UNION ALL
           SELECT t.id FROM todo t JOIN subtree s ON t.parent_id = s.id
         )
         UPDATE todo SET list_id = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id IN (SELECT id FROM subtree)",
        &[Value::Integer(id), Value::Integer(list_id)],
    )?;
    Ok(())
}

/// Puts a todo under a new parent, or at the top level, at the end of its new siblings.
///
/// @param tx - the open transaction
/// @param id - the todo
/// @param list_id - the list it is in after the change
/// @param parent - its new parent, or nothing for the top level
fn reparent(tx: &SharedTransaction, id: i64, list_id: i64, parent: Option<i64>) -> ApiResult<()> {
    tx_execute(
        tx,
        "UPDATE todo SET parent_id = ?2,
                position = (SELECT coalesce(max(position), 0) + 1 FROM todo WHERE list_id = ?3 AND parent_id IS ?2 AND id <> ?1)
         WHERE id = ?1",
        &[Value::Integer(id), optional_int(parent), Value::Integer(list_id)],
    )?;
    Ok(())
}

/// Writes the plain fields of a `PATCH` in one `UPDATE`.
///
/// The statement names only the columns that were sent, and always sets
/// `updated_at`. Each value is bound as a parameter, never pasted into the
/// SQL: only the column names, which come from this function, are.
///
/// @param tx - the open transaction
/// @param id - the todo
/// @param patch - the fields to change
fn update_fields(tx: &SharedTransaction, id: i64, patch: &TodoPatch) -> ApiResult<()> {
    let mut columns: Vec<(&str, Value)> = Vec::new();
    if let Some(title) = &patch.title {
        columns.push(("title", text(title.trim())));
    }
    if let Some(notes) = &patch.notes {
        columns.push(("notes", text(notes)));
    }
    if let Some(completed) = patch.completed {
        columns.push(("completed", Value::Integer(i64::from(completed))));
    }
    if let Some(priority) = patch.priority {
        columns.push(("priority", Value::Integer(priority)));
    }
    if let Some(due_on) = &patch.due_on {
        columns.push(("due_on", optional_text(due_on.as_deref())));
    }
    if let Some(assignee) = patch.assignee_id {
        columns.push(("assignee_id", optional_int(assignee)));
    }
    let mut params = vec![Value::Integer(id)];
    let mut assignments = vec!["updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')".to_string()];
    for (column, value) in columns {
        params.push(value);
        assignments.push(format!("{column} = ?{}", params.len()));
    }
    tx_execute(tx, &format!("UPDATE todo SET {} WHERE id = ?1", assignments.join(", ")), &params)?;
    Ok(())
}

/// Returns the ids of a todo and every subtask below it.
///
/// @param tx - the open transaction
/// @param id - the todo at the top
pub(super) fn subtree_ids(tx: &SharedTransaction, id: i64) -> ApiResult<Vec<i64>> {
    let rows = tx_query(
        tx,
        "WITH RECURSIVE subtree (id) AS (
           SELECT ?1
           UNION ALL
           SELECT t.id FROM todo t JOIN subtree s ON t.parent_id = s.id
         )
         SELECT id FROM subtree",
        &[Value::Integer(id)],
    )?;
    Ok(Record::all(&rows).map(|row| row.int("id")).collect())
}

/// Reads a field that may be absent, `null`, or a value, as `Option<Option<T>>`.
///
/// serde calls this only when the field is present, so a present `null`
/// becomes `Some(None)` and an absent field stays at its default, `None`.
///
/// @param deserializer - the JSON value
fn nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
