//! Questions across every list: keyword search, the agenda, and completions per day.

use inillucent::Value;
use serde::Serialize;

use super::todos::{card_from, CARD_SELECT};
use super::{optional_int, text, Record, Store, TodoCard};
use crate::error::{ApiError, ApiResult};

/// One search result.
#[derive(Debug, Serialize)]
pub struct SearchHit {
    /// The todo.
    #[serde(flatten)]
    pub todo: TodoCard,
    /// The BM25 score. Lower is a better match.
    pub score: f64,
    /// The title with each matched word in `[` and `]`.
    pub title_marked: String,
    /// Up to ten words of the notes around the match, marked the same way.
    pub notes_snippet: String,
}

/// One day of the agenda.
#[derive(Debug, Serialize)]
pub struct AgendaDay {
    /// The day, `YYYY-MM-DD`.
    pub day: String,
    /// The open todos due that day, most important first. Empty on a free day.
    pub todos: Vec<TodoCard>,
}

/// The agenda: what is late, then each day of the window.
#[derive(Debug, Serialize)]
pub struct Agenda {
    /// The first day of the window.
    pub from: String,
    /// Open todos due before `from`.
    pub overdue: Vec<TodoCard>,
    /// Every day of the window, including the days with nothing due.
    pub days: Vec<AgendaDay>,
}

/// How many todos were completed and reopened on one day.
#[derive(Debug, Serialize)]
pub struct CompletionDay {
    /// The day, `YYYY-MM-DD`.
    pub day: String,
    /// Todos completed that day.
    pub completed: i64,
    /// Todos reopened that day.
    pub reopened: i64,
    /// Todos completed from the first day of the window to this one.
    pub running_total: i64,
}

impl Store {
    /// Finds todos whose title or notes contain every word of a query.
    ///
    /// The search runs in the FTS5 table `todo_fts`, which is joined to the
    /// `todo_card` view by rowid, so each hit comes back as a whole todo with
    /// its list, assignee and tags. `bm25(todo_fts, 10.0, 1.0)` weights a match
    /// in the title ten times a match in the notes. `highlight` and `snippet`
    /// mark the matched words.
    ///
    /// @param query - the words to find, as a person typed them
    /// @param list_id - only this list, or every list
    /// @param limit - the most hits to return
    pub fn search(&self, query: &str, list_id: Option<i64>, limit: i64) -> ApiResult<Vec<SearchHit>> {
        let expression = match_expression(query).ok_or_else(|| ApiError::bad_request("the query has no words to search for"))?;
        let today = self.today(None)?;
        let rows = self.query(
            "SELECT c.*, (c.completed = 0 AND c.due_on < ?1) AS overdue,
                    bm25(todo_fts, 10.0, 1.0) AS score,
                    highlight(todo_fts, 0, '[', ']') AS title_marked,
                    snippet(todo_fts, 1, '[', ']', '...', 10) AS notes_snippet
             FROM todo_fts
             JOIN todo_card c ON c.id = todo_fts.rowid
             WHERE todo_fts MATCH ?2 AND (?3 IS NULL OR c.list_id = ?3)
             ORDER BY score
             LIMIT ?4",
            &[text(&today), text(&expression), optional_int(list_id), Value::Integer(limit.clamp(1, 100))],
        )?;
        Ok(Record::all(&rows)
            .map(|row| SearchHit {
                todo: card_from(&row),
                score: row.real("score"),
                title_marked: row.text("title_marked"),
                notes_snippet: row.text("notes_snippet"),
            })
            .collect())
    }

    /// Returns what is overdue, and what is due on each of the next `days` days.
    ///
    /// The days come from a recursive CTE that counts from `from` one day at a
    /// time, so a day with nothing due is still in the answer. The calendar is
    /// `LEFT JOIN`ed to the open todos: a free day gets one row whose todo
    /// columns are NULL, and a busy day gets one row per todo.
    ///
    /// The open todos are a derived table rather than a condition in the
    /// join's `ON` clause. That keeps each part readable, and it avoids the
    /// `LEFT JOIN` defect that `lists.rs` describes.
    ///
    /// @param from - the first day, or nothing for today
    /// @param days - how many days, from 1 to 60
    /// @param assignee_id - only this person's todos, or everyone's
    pub fn agenda(&self, from: Option<&str>, days: i64, assignee_id: Option<i64>) -> ApiResult<Agenda> {
        let from = self.today(from)?;
        let params = [text(&from), Value::Integer(days.clamp(1, 60)), optional_int(assignee_id)];
        let rows = self.query(
            "WITH RECURSIVE calendar (day) AS (
               SELECT ?1
               UNION ALL
               SELECT date(day, '+1 day') FROM calendar WHERE day < date(?1, printf('+%d days', ?2 - 1))
             )
             SELECT calendar.day AS agenda_day, c.*, 0 AS overdue
             FROM calendar
             LEFT JOIN (
               SELECT * FROM todo_card WHERE completed = 0 AND (?3 IS NULL OR assignee_id = ?3)
             ) c ON c.due_on = calendar.day
             ORDER BY calendar.day, c.priority, c.position, c.id",
            &params,
        )?;
        let mut agenda_days: Vec<AgendaDay> = Vec::new();
        for row in Record::all(&rows) {
            let day = row.text("agenda_day");
            if agenda_days.last().map(|last| last.day != day).unwrap_or(true) {
                agenda_days.push(AgendaDay { day, todos: Vec::new() });
            }
            if row.opt_int("id").is_some() {
                if let Some(last) = agenda_days.last_mut() {
                    last.todos.push(card_from(&row));
                }
            }
        }
        let overdue = self.query(
            &format!(
                "{CARD_SELECT} WHERE completed = 0 AND due_on < ?1 AND (?3 IS NULL OR assignee_id = ?3) ORDER BY due_on, priority, id"
            ),
            &params,
        )?;
        Ok(Agenda { from, overdue: Record::all(&overdue).map(|row| card_from(&row)).collect(), days: agenda_days })
    }

    /// Counts the todos completed and reopened on each of the last `days` days.
    ///
    /// The counts come from `activity`, which the `todo_completed` trigger
    /// writes, grouped by day in a derived table. A recursive CTE makes the
    /// calendar, so a day with no activity reports zeros. `sum(...) OVER
    /// (ORDER BY day)` adds each day's completions to the days before it.
    ///
    /// @param until - the last day, or nothing for today
    /// @param days - how many days, from 1 to 366
    pub fn completions(&self, until: Option<&str>, days: i64) -> ApiResult<Vec<CompletionDay>> {
        let until = self.today(until)?;
        let rows = self.query(
            "WITH RECURSIVE calendar (day) AS (
               SELECT date(?1, printf('-%d days', ?2 - 1))
               UNION ALL
               SELECT date(day, '+1 day') FROM calendar WHERE day < ?1
             )
             SELECT calendar.day,
                    coalesce(c.completed, 0) AS completed,
                    coalesce(c.reopened, 0) AS reopened,
                    sum(coalesce(c.completed, 0)) OVER (ORDER BY calendar.day) AS running_total
             FROM calendar
             LEFT JOIN (
               SELECT date(at) AS day,
                      count(*) FILTER (WHERE kind = 'completed') AS completed,
                      count(*) FILTER (WHERE kind = 'reopened') AS reopened
               FROM activity
               GROUP BY date(at)
             ) c ON c.day = calendar.day
             ORDER BY calendar.day",
            &[text(&until), Value::Integer(days.clamp(1, 366))],
        )?;
        Ok(Record::all(&rows)
            .map(|row| CompletionDay {
                day: row.text("day"),
                completed: row.int("completed"),
                reopened: row.int("reopened"),
                running_total: row.int("running_total"),
            })
            .collect())
    }
}

/// Turns what a person typed into an FTS5 query that matches every word as a prefix.
///
/// `paint fen` becomes `"paint"* "fen"*`, and FTS5 joins the terms with AND.
/// The text is cut into words at every character that is not a letter or a
/// digit, which is also where the `unicode61` tokenizer cuts it. That keeps
/// out `-`, `:`, `(`, `*` and `"`, which the FTS5 query language reads as
/// operators, so the search box cannot send a query FTS5 refuses as a syntax
/// error.
///
/// @param query - the text from the search box
fn match_expression(query: &str) -> Option<String> {
    let terms: Vec<String> =
        query.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).map(|word| format!("\"{word}\"*")).collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::match_expression;

    #[test]
    fn every_word_becomes_a_quoted_prefix() {
        assert_eq!(match_expression("paint fen").as_deref(), Some("\"paint\"* \"fen\"*"));
    }

    #[test]
    fn operators_are_not_passed_through() {
        assert_eq!(match_expression("to-do: (fence)*").as_deref(), Some("\"to\"* \"do\"* \"fence\"*"));
        assert_eq!(match_expression("say \"hi\"").as_deref(), Some("\"say\"* \"hi\"*"));
    }

    #[test]
    fn a_query_with_no_words_is_refused() {
        assert_eq!(match_expression("  -- ** "), None);
    }
}
