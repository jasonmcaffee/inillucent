//! Lists: the overview of every list, the todos in one, and the operations
//! TodoMVC has on a whole list: toggle all, clear completed, and reorder.

use inillucent::Value;
use serde::{Deserialize, Serialize};

use super::todos::{card_from, require_list, subtree_ids, CARD_SELECT};
use super::{text, tx_execute, tx_query, Record, Store, TodoCard};
use crate::error::{ApiError, ApiResult};

/// The body of `POST /lists`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewList {
    /// The person who owns it.
    pub owner_id: i64,
    /// Its name. Unique among the owner's lists.
    pub name: String,
}

/// The body of `PATCH /lists/{id}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListPatch {
    /// A new name.
    pub name: String,
}

/// A list with a summary of its top level todos.
#[derive(Debug, Serialize)]
pub struct ListOverview {
    /// The list's id.
    pub id: i64,
    /// Its name.
    pub name: String,
    /// The person who owns it.
    pub owner_id: i64,
    /// Their name.
    pub owner_name: String,
    /// When it was created.
    pub created_at: String,
    /// Top level todos.
    pub total: i64,
    /// Of those, the ones not done.
    pub open: i64,
    /// The ones done.
    pub done: i64,
    /// Open todos whose due date has passed.
    pub overdue: i64,
    /// The share done, from 0 to 100, or nothing for an empty list.
    pub percent_done: Option<f64>,
    /// The earliest due date among the open todos.
    pub next_due: Option<String>,
    /// 1 for the list with the most open todos. Lists with the same number share a rank.
    pub busiest_rank: i64,
}

/// The query string of `GET /lists/{id}/todos`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoFilter {
    /// `all`, `active` or `completed`, as in TodoMVC. `all` when left out.
    #[serde(default)]
    pub status: Option<String>,
    /// Only todos with this tag.
    #[serde(default)]
    pub tag: Option<String>,
    /// Only todos assigned to this person.
    #[serde(default)]
    pub assignee_id: Option<i64>,
    /// `position`, `due` or `priority`. `position` when left out.
    #[serde(default)]
    pub sort: Option<String>,
    /// Include subtasks as well as top level todos.
    #[serde(default)]
    pub subtasks: Option<bool>,
    /// The day that decides what is overdue. Today when left out.
    #[serde(default)]
    pub today: Option<String>,
}

/// The todos in a list, and the counts TodoMVC shows in its footer.
#[derive(Debug, Serialize)]
pub struct ListTodos {
    /// The todos that pass the filter.
    pub todos: Vec<TodoCard>,
    /// Top level todos in the list, whatever the filter.
    pub all: i64,
    /// Of those, the ones not done: TodoMVC's "items left".
    pub active: i64,
    /// The ones done: what "clear completed" would delete.
    pub completed: i64,
}

/// The overview columns.
///
/// Each list is joined to its top level todos with a `LEFT JOIN`, so a list
/// with no todos still has one row, whose todo columns are NULL. The
/// condition `t.parent_id IS NULL` is in the `ON` clause and not in `WHERE`:
/// in `WHERE` it would be tested after the join and would drop an empty
/// list's NULL row. `count(t.id)` counts only real todos, so an empty list
/// counts 0.
///
/// `rank()` is a window function over the grouped rows: it numbers the lists
/// by open todos, and two lists with the same number get the same rank.
const OVERVIEW_SELECT: &str = "
SELECT l.id, l.name, l.owner_id, o.name AS owner_name, l.created_at,
       count(t.id) AS total,
       count(t.id) FILTER (WHERE t.completed = 0) AS open,
       count(t.id) FILTER (WHERE t.completed = 1) AS done,
       count(t.id) FILTER (WHERE t.completed = 0 AND t.due_on < ?1) AS overdue,
       round(100.0 * count(t.id) FILTER (WHERE t.completed = 1) / count(t.id), 1) AS percent_done,
       min(t.due_on) FILTER (WHERE t.completed = 0) AS next_due,
       rank() OVER (ORDER BY count(t.id) FILTER (WHERE t.completed = 0) DESC) AS busiest_rank
FROM list l
JOIN person o ON o.id = l.owner_id
LEFT JOIN todo t ON t.list_id = l.id AND t.parent_id IS NULL
GROUP BY l.id, l.name, l.owner_id, o.name, l.created_at";

impl Store {
    /// Creates a list and returns its overview.
    ///
    /// @param list - the owner and the name
    pub fn create_list(&self, list: &NewList) -> ApiResult<ListOverview> {
        let rows = self.query(
            "INSERT INTO list (owner_id, name) VALUES (?1, ?2) RETURNING id",
            &[Value::Integer(list.owner_id), text(list.name.trim())],
        )?;
        let id = Record::first(&rows).map(|row| row.int("id")).ok_or_else(|| ApiError::conflict("the list was not added"))?;
        self.list(id, None)
    }

    /// Returns every list with its summary, the busiest first.
    ///
    /// @param today - the day that decides what is overdue
    pub fn lists(&self, today: Option<&str>) -> ApiResult<Vec<ListOverview>> {
        let today = self.today(today)?;
        let rows = self.query(&format!("{OVERVIEW_SELECT} ORDER BY busiest_rank, l.name, l.id"), &[text(&today)])?;
        Ok(Record::all(&rows).map(|row| overview_from(&row)).collect())
    }

    /// Returns one list with its summary.
    ///
    /// The window function has to see every list to rank this one, so a
    /// `WHERE l.id = ?` would rank the list against itself alone. Every list
    /// is ranked in a derived table, and the outer query picks the one row.
    ///
    /// @param id - the list's id
    /// @param today - the day that decides what is overdue
    pub fn list(&self, id: i64, today: Option<&str>) -> ApiResult<ListOverview> {
        let today = self.today(today)?;
        let rows = self.query(&format!("SELECT * FROM ({OVERVIEW_SELECT}) WHERE id = ?2"), &[text(&today), Value::Integer(id)])?;
        Record::first(&rows).map(|row| overview_from(&row)).ok_or_else(|| ApiError::not_found(format!("list {id}")))
    }

    /// Renames a list.
    ///
    /// @param id - the list's id
    /// @param patch - the new name
    pub fn rename_list(&self, id: i64, patch: &ListPatch) -> ApiResult<ListOverview> {
        match self.execute("UPDATE list SET name = ?2 WHERE id = ?1", &[Value::Integer(id), text(patch.name.trim())])? {
            0 => Err(ApiError::not_found(format!("list {id}"))),
            _ => self.list(id, None),
        }
    }

    /// Deletes a list and every todo in it, and returns how many todos went.
    ///
    /// `ON DELETE CASCADE` deletes the todos, and through them their tags
    /// links and comments. The `todo_fts_delete` trigger fires for each todo
    /// the cascade removes, so the search entries go too.
    ///
    /// @param id - the list's id
    pub fn delete_list(&self, id: i64) -> ApiResult<u64> {
        let tx = self.begin()?;
        require_list(&tx, id)?;
        let rows = tx_query(&tx, "SELECT count(*) FROM todo WHERE list_id = ?1", &[Value::Integer(id)])?;
        let todos = Record::first(&rows).map(|row| row.int_at(0)).unwrap_or(0);
        tx_execute(&tx, "DELETE FROM list WHERE id = ?1", &[Value::Integer(id)])?;
        tx.commit()?;
        Ok(todos as u64)
    }

    /// Returns the todos in a list that pass a filter, and the list's counts.
    ///
    /// The todos come from the `todo_card` view. The filter becomes `AND`
    /// conditions with bound values, and the tag filter is an `EXISTS` over
    /// the tag join, so a todo with three tags is still returned once.
    ///
    /// @param list_id - the list
    /// @param filter - the query string
    pub fn list_todos(&self, list_id: i64, filter: &TodoFilter) -> ApiResult<ListTodos> {
        let today = self.today(filter.today.as_deref())?;
        let summary = self.list(list_id, Some(&today))?;
        let mut params = vec![text(&today), Value::Integer(list_id)];
        let mut conditions = vec!["list_id = ?2".to_string()];
        if !filter.subtasks.unwrap_or(false) {
            conditions.push("parent_id IS NULL".to_string());
        }
        match filter.status.as_deref().unwrap_or("all") {
            "all" => {}
            "active" => conditions.push("completed = 0".to_string()),
            "completed" => conditions.push("completed = 1".to_string()),
            other => return Err(ApiError::bad_request(format!("status `{other}` is not one of all, active, completed"))),
        }
        if let Some(tag) = &filter.tag {
            params.push(text(tag));
            conditions.push(format!(
                "EXISTS (SELECT 1 FROM todo_tag tt JOIN tag g ON g.id = tt.tag_id WHERE tt.todo_id = todo_card.id AND g.name = ?{})",
                params.len()
            ));
        }
        if let Some(assignee) = filter.assignee_id {
            params.push(Value::Integer(assignee));
            conditions.push(format!("assignee_id = ?{}", params.len()));
        }
        let order = match filter.sort.as_deref().unwrap_or("position") {
            "position" => "parent_id IS NOT NULL, position, id",
            "due" => "due_on IS NULL, due_on, priority, id",
            "priority" => "priority, due_on IS NULL, due_on, id",
            other => return Err(ApiError::bad_request(format!("sort `{other}` is not one of position, due, priority"))),
        };
        let sql = format!("{CARD_SELECT} WHERE {} ORDER BY {order}", conditions.join(" AND "));
        let rows = self.query(&sql, &params)?;
        Ok(ListTodos {
            todos: Record::all(&rows).map(|row| card_from(&row)).collect(),
            all: summary.total,
            active: summary.open,
            completed: summary.done,
        })
    }

    /// Marks every todo in a list done, or every one not done, and returns how many changed.
    ///
    /// One `UPDATE`. The `todo_completed` trigger fires once for each row it
    /// changes, stamping `completed_at` and writing the timeline, and the
    /// `WHERE completed <> ?2` means a todo already in that state is not
    /// touched and gets no timeline entry.
    ///
    /// @param list_id - the list
    /// @param completed - true to complete them all, false to reopen them all
    pub fn toggle_all(&self, list_id: i64, completed: bool) -> ApiResult<u64> {
        let tx = self.begin()?;
        require_list(&tx, list_id)?;
        let changed = tx_execute(
            &tx,
            "UPDATE todo SET completed = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE list_id = ?1 AND completed <> ?2",
            &[Value::Integer(list_id), Value::Integer(i64::from(completed))],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// Deletes every completed todo in a list, and returns how many todos went.
    ///
    /// A completed todo's subtasks go with it, done or not, through `ON DELETE
    /// CASCADE`, and the count includes them. The `todo_fts_delete` trigger
    /// removes the search entry of every todo that goes.
    ///
    /// @param list_id - the list
    pub fn clear_completed(&self, list_id: i64) -> ApiResult<u64> {
        let tx = self.begin()?;
        require_list(&tx, list_id)?;
        let rows = tx_query(&tx, "SELECT id FROM todo WHERE list_id = ?1 AND completed = 1", &[Value::Integer(list_id)])?;
        let mut gone: Vec<i64> = Vec::new();
        for row in Record::all(&rows) {
            for id in subtree_ids(&tx, row.int("id"))? {
                if !gone.contains(&id) {
                    gone.push(id);
                }
            }
        }
        tx_execute(&tx, "DELETE FROM todo WHERE list_id = ?1 AND completed = 1", &[Value::Integer(list_id)])?;
        tx.commit()?;
        Ok(gone.len() as u64)
    }

    /// Puts a list's top level todos in a new order.
    ///
    /// The request must name every top level todo in the list exactly once,
    /// so a client working from an old copy of the list cannot lose a todo.
    /// The check and the writes are in one transaction.
    ///
    /// The positions are written with one `UPDATE ... FROM`. The order goes in
    /// as one JSON array, `json_each` turns it into rows of index and id, and
    /// each todo takes its index plus one as its position.
    ///
    /// @param list_id - the list
    /// @param order - the todo ids, first to last
    pub fn reorder(&self, list_id: i64, order: &[i64]) -> ApiResult<Vec<TodoCard>> {
        let tx = self.begin()?;
        require_list(&tx, list_id)?;
        let rows = tx_query(&tx, "SELECT id FROM todo WHERE list_id = ?1 AND parent_id IS NULL ORDER BY id", &[Value::Integer(list_id)])?;
        let mut current: Vec<i64> = Record::all(&rows).map(|row| row.int("id")).collect();
        let mut requested = order.to_vec();
        requested.sort_unstable();
        current.sort_unstable();
        if requested != current {
            return Err(ApiError::conflict(format!(
                "the order must name every top level todo in list {list_id} once: expected {current:?}, got {order:?}"
            )));
        }
        let ids: Vec<String> = order.iter().map(i64::to_string).collect();
        tx_execute(
            &tx,
            "UPDATE todo SET position = p.pos
             FROM (SELECT value AS id, key + 1 AS pos FROM json_each(?1)) AS p
             WHERE todo.id = p.id",
            &[text(&format!("[{}]", ids.join(",")))],
        )?;
        tx.commit()?;
        Ok(self.list_todos(list_id, &TodoFilter::default())?.todos)
    }
}

/// Builds a [`ListOverview`] from a row of `OVERVIEW_SELECT`.
///
/// @param row - the row
fn overview_from(row: &Record) -> ListOverview {
    ListOverview {
        id: row.int("id"),
        name: row.text("name"),
        owner_id: row.int("owner_id"),
        owner_name: row.text("owner_name"),
        created_at: row.text("created_at"),
        total: row.int("total"),
        open: row.int("open"),
        done: row.int("done"),
        overdue: row.int("overdue"),
        percent_done: row.opt_int("total").filter(|total| *total > 0).map(|_| row.real("percent_done")),
        next_due: row.opt_text("next_due"),
        busiest_rank: row.int("busiest_rank"),
    }
}
