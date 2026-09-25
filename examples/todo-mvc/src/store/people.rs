//! People: the owners of lists, the assignees of todos, and the authors of comments.

use inillucent::Value;
use serde::{Deserialize, Serialize};

use super::{text, Record, Store};
use crate::error::{ApiError, ApiResult};

/// The body of `POST /people`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewPerson {
    /// The person's name.
    pub name: String,
    /// Their email address. Unique, ignoring case.
    pub email: String,
}

/// A person, with how many todos they have.
#[derive(Debug, Serialize)]
pub struct Person {
    /// The person's id.
    pub id: i64,
    /// Their name.
    pub name: String,
    /// Their email address.
    pub email: String,
    /// When they were added.
    pub created_at: String,
    /// Todos assigned to them and not done.
    pub open_todos: i64,
    /// Of those, the ones whose due date has passed.
    pub overdue_todos: i64,
    /// Todos assigned to them and done.
    pub done_todos: i64,
}

/// One open todo in a person's workload, in the order they should do them.
#[derive(Debug, Serialize)]
pub struct WorkloadItem {
    /// 1 for the todo to do first.
    pub rank: i64,
    /// The todo's id.
    pub todo_id: i64,
    /// The todo's title.
    pub title: String,
    /// The list it is in.
    pub list_name: String,
    /// 1 is high, 3 is low.
    pub priority: i64,
    /// The due date, if it has one.
    pub due_on: Option<String>,
    /// Whether the due date has passed.
    pub overdue: bool,
    /// How many of this person's open todos are due on or before this one's day.
    /// Nothing for a todo with no due date.
    pub due_by_then: Option<i64>,
    /// How many of this person's open todos are in the same list.
    pub in_same_list: i64,
}

/// The columns of `person` plus the three counts, shared by the list and the single read.
///
/// `count(t.id) FILTER (WHERE ...)` counts only the joined rows that pass the
/// filter. One `LEFT JOIN` and three filtered counts replace three separate
/// queries, and a person with no todos still gets a row, with zeros.
const PERSON_SELECT: &str = "
SELECT p.id, p.name, p.email, p.created_at,
       count(t.id) FILTER (WHERE t.completed = 0) AS open_todos,
       count(t.id) FILTER (WHERE t.completed = 0 AND t.due_on < ?1) AS overdue_todos,
       count(t.id) FILTER (WHERE t.completed = 1) AS done_todos
FROM person p
LEFT JOIN todo t ON t.assignee_id = p.id";

impl Store {
    /// Adds a person and returns them.
    ///
    /// An email that is already taken, in any case, is refused by the `UNIQUE
    /// COLLATE NOCASE` constraint, and the handler answers `409`.
    ///
    /// @param person - the name and email
    pub fn create_person(&self, person: &NewPerson) -> ApiResult<Person> {
        let rows =
            self.query("INSERT INTO person (name, email) VALUES (?1, ?2) RETURNING id", &[text(&person.name), text(person.email.trim())])?;
        let id = Record::first(&rows).map(|row| row.int("id")).ok_or_else(|| ApiError::conflict("the person was not added"))?;
        self.person(id, None)
    }

    /// Returns every person, by name, with their todo counts.
    ///
    /// @param today - the day that decides what is overdue
    pub fn people(&self, today: Option<&str>) -> ApiResult<Vec<Person>> {
        let today = self.today(today)?;
        let rows = self.query(&format!("{PERSON_SELECT} GROUP BY p.id ORDER BY p.name, p.id"), &[text(&today)])?;
        Ok(Record::all(&rows).map(|row| person_from(&row)).collect())
    }

    /// Returns one person with their todo counts.
    ///
    /// @param id - the person's id
    /// @param today - the day that decides what is overdue
    pub fn person(&self, id: i64, today: Option<&str>) -> ApiResult<Person> {
        let today = self.today(today)?;
        let rows = self.query(&format!("{PERSON_SELECT} WHERE p.id = ?2 GROUP BY p.id"), &[text(&today), Value::Integer(id)])?;
        Record::first(&rows).map(|row| person_from(&row)).ok_or_else(|| ApiError::not_found(format!("person {id}")))
    }

    /// Deletes a person.
    ///
    /// The foreign keys do the rest: their lists and every todo in them go
    /// (`ON DELETE CASCADE`), and todos in other people's lists that were
    /// assigned to them become unassigned (`ON DELETE SET NULL`).
    ///
    /// @param id - the person's id
    pub fn delete_person(&self, id: i64) -> ApiResult<()> {
        match self.execute("DELETE FROM person WHERE id = ?1", &[Value::Integer(id)])? {
            0 => Err(ApiError::not_found(format!("person {id}"))),
            _ => Ok(()),
        }
    }

    /// Returns a person's open todos, ranked in the order to do them.
    ///
    /// Three window functions run over the same rows, each with its own window:
    ///
    /// - `row_number()` over every row, ordered by priority and then due date,
    ///   numbers the todos 1, 2, 3 in the order to do them.
    /// - `count(*) OVER (ORDER BY due)` is a running count: with an `ORDER BY`
    ///   and no frame, the window is every row up to and including this one's
    ///   due date, so it says how many todos are due by then.
    /// - `count(*) OVER (PARTITION BY list_id)` counts the rows in the same
    ///   list, without a `GROUP BY` that would collapse them into one row.
    ///
    /// `t.due_on IS NULL` sorts first as 0 or 1, which puts todos with no due
    /// date after every dated one.
    ///
    /// @param id - the person's id
    /// @param today - the day that decides what is overdue
    pub fn workload(&self, id: i64, today: Option<&str>) -> ApiResult<Vec<WorkloadItem>> {
        let today = self.today(today)?;
        self.person(id, Some(&today))?;
        let rows = self.query(
            "SELECT t.id, t.title, l.name AS list_name, t.priority, t.due_on,
                    t.due_on < ?2 AS overdue,
                    row_number() OVER (ORDER BY t.priority, t.due_on IS NULL, t.due_on, t.id) AS rank,
                    CASE WHEN t.due_on IS NOT NULL
                         THEN count(*) OVER (ORDER BY t.due_on IS NULL, t.due_on) END AS due_by_then,
                    count(*) OVER (PARTITION BY t.list_id) AS in_same_list
             FROM todo t
             JOIN list l ON l.id = t.list_id
             WHERE t.assignee_id = ?1 AND t.completed = 0
             ORDER BY rank",
            &[Value::Integer(id), text(&today)],
        )?;
        Ok(Record::all(&rows)
            .map(|row| WorkloadItem {
                rank: row.int("rank"),
                todo_id: row.int("id"),
                title: row.text("title"),
                list_name: row.text("list_name"),
                priority: row.int("priority"),
                due_on: row.opt_text("due_on"),
                overdue: row.bool("overdue"),
                due_by_then: row.opt_int("due_by_then"),
                in_same_list: row.int("in_same_list"),
            })
            .collect())
    }
}

/// Builds a [`Person`] from a row of `PERSON_SELECT`.
///
/// @param row - the row
fn person_from(row: &Record) -> Person {
    Person {
        id: row.int("id"),
        name: row.text("name"),
        email: row.text("email"),
        created_at: row.text("created_at"),
        open_todos: row.int("open_todos"),
        overdue_todos: row.int("overdue_todos"),
        done_todos: row.int("done_todos"),
    }
}
