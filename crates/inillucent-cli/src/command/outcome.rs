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
//! sentence: it is one of the driver's thirteen status names, so an MCP client,
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

    /// Adds what opening the database did, when it did anything.
    ///
    /// **Only when something happened (task-1979, C10).** A `recovered` member
    /// on every result of every command would change the envelope every caller
    /// parses and every page documents, to carry `false` almost always. An
    /// operator investigating a crash asks one question - was this file
    /// recovered - and the answer is the presence of the member.
    ///
    /// A stray segment is reported the same way and for the same reason: it is
    /// a file beside the database that nothing will replay and nothing will
    /// remove, so naming it is the only way anybody finds out it is there.
    ///
    /// @param recovery - what the open reported
    /// @param strays - segments beside the file the chain does not reach
    pub fn with_recovery(
        mut self,
        recovery: &inillucent_driver::Recovery,
        strays: &[u64],
    ) -> Outcome {
        // **A dropped record is reported even when nothing else was**
        // (task-2066 §4.1.10). `recovered` is false for an open that had no work
        // to restore, and a drop can happen on one of those - so the condition
        // is either, not just the first.
        if recovery.recovered || recovery.dropped > 0 {
            self.extra.push((
                "recovered".to_string(),
                json::object(vec![
                    ("records_scanned", Json::Int(recovery.scanned as i64)),
                    ("records_applied", Json::Int(recovery.applied as i64)),
                    ("records_dropped", Json::Int(recovery.dropped as i64)),
                    (
                        "transactions_committed",
                        Json::Int(recovery.committed as i64),
                    ),
                    ("transactions_discarded", Json::Int(recovery.losers as i64)),
                ]),
            ));
            // **"Recovered" only when there was something to recover from.**
            // Replaying committed transactions the file has not taken yet is
            // what every open does while another process has the file open
            // and has not checkpointed, and printing "recovered the log" for
            // that on every command told a person the database had been
            // damaged when nothing had happened to it. A transaction with no
            // commit record, or a dropped record, is what a crash leaves, and
            // that keeps the word. The JSON member above is unchanged either
            // way, so a caller asking whether the log was replayed still can.
            let said = if recovery.losers > 0 || recovery.dropped > 0 {
                format!(
                    "recovered the log: {} records scanned, {} applied, {} transactions committed, {} discarded.",
                    recovery.scanned, recovery.applied, recovery.committed, recovery.losers
                )
            } else {
                format!(
                    "replayed the log: {} committed transactions were in the log and not yet in the \
                     database file ({} records scanned, {} applied). A connection that still has the \
                     file open, or one that ended before a checkpoint, wrote them.",
                    recovery.committed, recovery.scanned, recovery.applied
                )
            };
            self.text = format!(
                "{}{}{said}",
                self.text,
                if self.text.is_empty() { "" } else { "\n" },
            );
        }
        // On its own line and only when it happened, because it is the one
        // number here that means somebody should look.
        if recovery.dropped > 0 {
            self.text = format!(
                    "{}
{} log record(s) were DROPPED: they name a tree this recovery had no                      shape for. That is usually a table dropped inside the replayed window, and                      it is how task-1932 and task-2033 both lost rows silently. Run                      `inillucent integrity-check` and compare the row counts you expect.",
                    self.text, recovery.dropped
                );
        }
        if !strays.is_empty() {
            let named: Vec<Json> = strays
                .iter()
                .map(|sequence| Json::Int(*sequence as i64))
                .collect();
            self.extra
                .push(("stray_log_segments".to_string(), Json::Array(named)));
            let listed = strays
                .iter()
                .map(|sequence| format!("{sequence:010}"))
                .collect::<Vec<String>>()
                .join(", ");
            self.text = format!(
                "{}{}log segments beside this database that its chain does not reach: {listed}. Nothing replays them.",
                self.text,
                if self.text.is_empty() { "" } else { "\n" }
            );
        }
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

    /// A replay of committed transactions is not reported as a recovery, and
    /// a discarded transaction still is.
    ///
    /// **The first open while another process has the file open replays that
    /// process's committed transactions**, and every command printed
    /// "recovered the log" for it, which reads as damage. The JSON member is
    /// the same in both cases, so a caller asking whether the log was replayed
    /// still can.
    #[test]
    fn only_a_discarded_transaction_is_called_a_recovery() {
        let routine = inillucent_driver::Recovery {
            recovered: true,
            scanned: 7,
            applied: 4,
            committed: 3,
            ..Default::default()
        };
        let replayed = Outcome::said("query", "").with_recovery(&routine, &[]);
        assert!(
            replayed
                .text
                .starts_with("replayed the log: 3 committed transactions"),
            "{}",
            replayed.text
        );
        assert!(!replayed.text.contains("recovered"), "{}", replayed.text);
        assert!(replayed.extra.iter().any(|(name, _)| name == "recovered"));

        let crashed = inillucent_driver::Recovery {
            losers: 1,
            ..routine
        };
        let recovered = Outcome::said("query", "").with_recovery(&crashed, &[]);
        assert!(
            recovered
                .text
                .starts_with("recovered the log: 7 records scanned, 4 applied, 3 transactions committed, 1 discarded."),
            "{}",
            recovered.text
        );
    }
}
