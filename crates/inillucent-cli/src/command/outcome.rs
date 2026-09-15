//! What a command produced, and the two ways it is read.
//!
//! Invariant: **the human rendering and the machine rendering come from one
//! run.** An [`Outcome`] carries both - `text` for a person or an agent reading
//! a terminal, and the structured fields for a script - and every command fills
//! them in the same call. A front end that had to run the command twice to get
//! the other view would be able to get two different answers, and the one it
//! showed would be the one nobody checked.
//!
//! The failure side is [`Failed`], and its `status` is deliberately not a
//! sentence: it is one of the driver's fourteen status names, so an MCP client,
//! a shell script and a C binding all read the same word for the same class of
//! failure. `drivers/README.md` argues the case for `unsupported` being its own
//! status rather than a flavour of syntax error; this is that argument holding
//! in a third front end.

use inillucent_driver::Status;

use crate::json::{self, Json};

/// Why a command did not succeed.
#[derive(Debug, Clone)]
pub struct Failed {
    /// Which class of failure it is, as the driver names them.
    pub status: Status,
    /// What went wrong, in a sentence.
    pub message: String,
    /// The construct the engine has not built, when the status is `unsupported`.
    pub feature: Option<String>,
    /// Where in the statement, when the failure knows.
    pub offset: Option<u32>,
}

impl Failed {
    /// Builds a failure of a given class.
    ///
    /// @param status - the class
    /// @param message - what went wrong
    pub fn said(status: Status, message: impl Into<String>) -> Failed {
        Failed {
            status,
            message: message.into(),
            feature: None,
            offset: None,
        }
    }

    /// Builds the failure a caller's own mistake deserves.
    ///
    /// @param message - what was wrong with the request
    pub fn misuse(message: impl Into<String>) -> Failed {
        Failed::said(Status::InvalidState, message)
    }

    /// Builds the failure for something this engine has not built.
    ///
    /// @param feature - the construct, named so a caller can act on it
    /// @param message - the sentence to show
    pub fn unsupported(feature: impl Into<String>, message: impl Into<String>) -> Failed {
        let feature = feature.into();
        Failed {
            status: Status::Unsupported,
            message: message.into(),
            feature: Some(feature),
            offset: None,
        }
    }

    /// Classifies an engine error the way the driver does.
    ///
    /// **The classification is the driver's, called rather than copied.** There
    /// is exactly one place in this repository that decides whether a refusal
    /// is `unsupported` or `syntax`, and a second copy of that decision here
    /// would drift the first time the engine grew a construct.
    ///
    /// @param error - the engine's error
    pub fn from_engine(error: &inillucent_base::DbError) -> Failed {
        let classified = inillucent_driver::Error::from_engine(error, false);
        Failed {
            status: classified.status,
            message: classified.message,
            feature: classified.feature,
            offset: classified.offset,
        }
    }

    /// Carries a failure the driver already classified.
    ///
    /// **For the callers that now go through the driver rather than the engine
    /// (task-1962, roadmap item 7).** `from_engine` above asks the driver to
    /// classify a raw engine error; this one is handed the answer, which is the
    /// same decision made in the same place.
    ///
    /// @param error - the driver's error
    pub fn from_driver(error: inillucent_driver::Error) -> Failed {
        Failed {
            status: error.status,
            message: error.message,
            feature: error.feature,
            offset: error.offset,
        }
    }

    /// Classifies a shell failure, which may or may not carry the engine's error.
    ///
    /// @param failure - what the shell reported
    pub fn from_shell(failure: &crate::shell::Failure) -> Failed {
        match failure.error.as_ref() {
            Some(error) => Failed::from_engine(error),
            // A failure with no error behind it came from the shell rather than
            // the engine - a dot command's own complaint - and `syntax` is the
            // honest reading of that: the request was not one the shell could
            // carry out as written.
            None => Failed {
                status: Status::Syntax,
                message: failure.message.clone(),
                feature: None,
                offset: failure.offset,
            },
        }
    }

    /// Renders this failure as the object every front end reports.
    ///
    /// @param command - the verb that failed
    pub fn to_json(&self, command: &str) -> Json {
        let mut pairs = vec![
            ("ok", Json::Bool(false)),
            ("command", json::text(command)),
            ("status", json::text(self.status.name())),
            ("message", json::text(&self.message)),
        ];
        if let Some(feature) = &self.feature {
            pairs.push(("feature", json::text(feature)));
        }
        if let Some(offset) = self.offset {
            pairs.push(("offset", Json::Int(i64::from(offset))));
        }
        pairs.push(("text", json::text(self.to_text())));
        json::object(pairs)
    }

    /// Renders this failure the way a person reads it.
    pub fn to_text(&self) -> String {
        let mut line = format!("Error [{}]: {}", self.status.name(), self.message);
        if let Some(feature) = &self.feature {
            line.push_str(&format!("\n  not built yet: {feature}"));
        }
        if let Some(offset) = self.offset {
            line.push_str(&format!("\n  at byte {offset} of the statement"));
        }
        line
    }

    /// Returns the process exit code this failure deserves.
    ///
    /// Three for `unsupported`, so a script can branch on "this engine has not
    /// built that" without matching on a message; one for everything else.
    pub fn exit_code(&self) -> i32 {
        match self.status {
            Status::Unsupported => 3,
            _ => 1,
        }
    }
}

/// One result column: its name, and what the values under it turned out to be.
#[derive(Debug, Clone)]
pub struct Column {
    /// The name the statement gave it.
    pub name: String,
    /// The storage class of its values.
    ///
    /// **Observed, not declared, and the word is chosen for that.** SQLite's
    /// typing is dynamic, so a column's declared affinity is a hint about what
    /// it will store rather than a fact about what it did. This is the fact:
    /// the class every non-null value in the result actually had, `mixed` when
    /// they disagreed and `null` when there were none.
    pub kind: String,
}

/// What a command produced.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The verb that ran.
    pub command: String,
    /// The result columns, empty for a command that produces no table.
    pub columns: Vec<Column>,
    /// The rows, already cut to whatever limit was asked for.
    pub rows: Vec<Vec<Json>>,
    /// How many rows the statement produced before the limit.
    pub total: usize,
    /// Whether the limit cut anything off.
    pub more: bool,
    /// How many rows the statement changed.
    pub changes: i64,
    /// The rowid the last insert assigned.
    pub last_insert_rowid: i64,
    /// How long it took.
    pub elapsed_ms: f64,
    /// The rendering a person reads.
    pub text: String,
    /// Anything this particular command has to say that the shape above cannot.
    pub extra: Vec<(String, Json)>,
}

impl Outcome {
    /// Builds an outcome that is only a message.
    ///
    /// @param command - the verb
    /// @param text - what to show
    pub fn said(command: &str, text: impl Into<String>) -> Outcome {
        Outcome {
            command: command.to_string(),
            columns: Vec::new(),
            rows: Vec::new(),
            total: 0,
            more: false,
            changes: 0,
            last_insert_rowid: 0,
            elapsed_ms: 0.0,
            text: text.into(),
            extra: Vec::new(),
        }
    }

    /// Adds a field only this command has.
    ///
    /// @param name - the member name
    /// @param value - what to put under it
    pub fn with(mut self, name: &str, value: Json) -> Outcome {
        self.extra.push((name.to_string(), value));
        self
    }

    /// Renders this outcome as the object every front end reports.
    pub fn to_json(&self) -> Json {
        let columns = self
            .columns
            .iter()
            .map(|column| {
                json::object(vec![
                    ("name", json::text(&column.name)),
                    ("type", json::text(&column.kind)),
                ])
            })
            .collect();
        let rows = self
            .rows
            .iter()
            .map(|row| Json::Array(row.clone()))
            .collect();
        let mut pairs = vec![
            ("ok", Json::Bool(true)),
            ("command", json::text(&self.command)),
            ("columns", Json::Array(columns)),
            ("rows", Json::Array(rows)),
            ("row_count", Json::Int(self.rows.len() as i64)),
            ("total", Json::Int(self.total as i64)),
            ("more", Json::Bool(self.more)),
            ("changes", Json::Int(self.changes)),
            ("last_insert_rowid", Json::Int(self.last_insert_rowid)),
            ("elapsed_ms", Json::Real(self.elapsed_ms)),
        ];
        let mut object = json::object(std::mem::take(&mut pairs));
        if let Json::Object(members) = &mut object {
            for (name, value) in &self.extra {
                members.push((name.clone(), value.clone()));
            }
            members.push(("text".to_string(), json::text(&self.text)));
        }
        object
    }
}

/// Returns the storage class of one JSON value, as a column type name.
///
/// @param value - the cell
fn class_of(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Int(_) => "integer",
        Json::Real(_) => "real",
        Json::Text(_) => "text",
        Json::Bool(_) => "integer",
        Json::Array(_) | Json::Object(_) => "blob",
    }
}

/// Works out each column's storage class from the rows under it.
///
/// @param names - the column names, in order
/// @param rows - every row of the result
pub fn columns_from(names: &[String], rows: &[Vec<Json>]) -> Vec<Column> {
    names
        .iter()
        .enumerate()
        .map(|(nth, name)| {
            let mut kind: Option<&'static str> = None;
            for row in rows {
                let Some(value) = row.get(nth) else { continue };
                if matches!(value, Json::Null) {
                    continue;
                }
                let seen = class_of(value);
                kind = match kind {
                    None => Some(seen),
                    Some(previous) if previous == seen => Some(previous),
                    Some(_) => Some("mixed"),
                };
            }
            Column {
                name: name.clone(),
                kind: kind.unwrap_or("null").to_string(),
            }
        })
        .collect()
}

/// Draws a result as an aligned table, which is what a person and a model read.
///
/// **Aligned rather than JSON by default, and that is a measured choice.** §6 of
/// the ticket's TDD records it: the local 27B model answers questions about a
/// table it can see the shape of, and loses columns out of a JSON array of
/// objects. The JSON is one parameter away for anything that would rather have
/// it.
///
/// @param columns - the result columns
/// @param rows - the rows to draw
/// @param null - what to show where a value is null
pub fn table(columns: &[Column], rows: &[Vec<Json>], null: &str) -> String {
    if columns.is_empty() {
        return String::new();
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .enumerate()
                .map(|(nth, _)| match row.get(nth) {
                    Some(Json::Null) | None => null.to_string(),
                    Some(Json::Text(text)) => text.clone(),
                    Some(other) => other.write(),
                })
                .collect()
        })
        .collect();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(nth, column)| {
            let widest = cells
                .iter()
                .filter_map(|row| row.get(nth))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0);
            widest.max(column.name.chars().count())
        })
        .collect();
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(join_padded(
        &columns
            .iter()
            .map(|column| column.name.clone())
            .collect::<Vec<_>>(),
        &widths,
    ));
    lines.push(
        widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>()
            .join("  "),
    );
    for row in &cells {
        lines.push(join_padded(row, &widths));
    }
    lines.join("\n")
}

/// Joins one row's cells, each padded to its column's width.
///
/// @param cells - the values, already rendered
/// @param widths - the width of each column
fn join_padded(cells: &[String], widths: &[usize]) -> String {
    let padded: Vec<String> = cells
        .iter()
        .enumerate()
        .map(|(nth, cell)| {
            let width = widths.get(nth).copied().unwrap_or(0);
            let short = width.saturating_sub(cell.chars().count());
            format!("{cell}{}", " ".repeat(short))
        })
        .collect();
    padded.join("  ").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A column's type is what its values were, not what was declared.
    #[test]
    fn a_column_type_is_observed() {
        let names = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let rows = vec![
            vec![Json::Int(1), Json::Null, Json::Null],
            vec![json::text("x"), json::text("y"), Json::Null],
        ];
        let columns = columns_from(&names, &rows);
        assert_eq!(columns[0].kind, "mixed");
        assert_eq!(columns[1].kind, "text");
        assert_eq!(columns[2].kind, "null");
    }

    /// The table pads to the widest cell and keeps the header over its column.
    #[test]
    fn the_table_lines_up() {
        let columns = columns_from(&["id".to_string(), "name".to_string()], &[]);
        let rows = vec![
            vec![Json::Int(1), json::text("Ada")],
            vec![Json::Int(1000), json::text("B")],
        ];
        let drawn = table(&columns, &rows, "");
        let lines: Vec<&str> = drawn.lines().collect();
        assert_eq!(lines[0], "id    name");
        assert_eq!(lines[1], "----  ----");
        assert_eq!(lines[2], "1     Ada");
        assert_eq!(lines[3], "1000  B");
    }

    /// A null is the placeholder, and never an empty string pretending to be one.
    #[test]
    fn a_null_is_drawn_as_the_placeholder() {
        let columns = columns_from(&["a".to_string()], &[]);
        let drawn = table(&columns, &[vec![Json::Null]], "NULL");
        assert!(drawn.contains("NULL"));
    }

    /// Unsupported gets its own exit code so a script can branch on it.
    #[test]
    fn unsupported_exits_three() {
        assert_eq!(Failed::unsupported("vacuum", "not built").exit_code(), 3);
        assert_eq!(Failed::misuse("nope").exit_code(), 1);
    }

    /// The outcome's JSON carries both views, and `text` is last.
    #[test]
    fn the_outcome_carries_both_views() {
        let outcome = Outcome::said("version", "0.1.0").with("engine", json::text("inillucent"));
        let written = outcome.to_json().write();
        assert!(written.starts_with("{\"ok\":true,\"command\":\"version\""));
        assert!(written.contains("\"engine\":\"inillucent\""));
        assert!(written.ends_with("\"text\":\"0.1.0\"}"));
    }
}
