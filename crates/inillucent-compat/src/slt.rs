//! The SQLLogicTest file format: reading it, writing it, and rendering values
//! into it.
//!
//! Invariant: the expected values in a `.test` file were produced by the pinned
//! SQLite binary and are checked in. A run without the oracle still grades
//! inillucent against SQLite's answers, because the answers are in the file rather
//! than being computed by whichever engine happens to be present.
//!
//! The format is SQLLogicTest's own, so an upstream file can be dropped into
//! `tests/conformance/` and run by the same code: `statement ok`, `query
//! <types> <sort> [label]`, a `----` separator, then one value per line with
//! `NULL` for a null and `(empty)` for an empty string. The type letters are
//! `T` for text, `I` for integer and `R` for real, and a value that does not
//! match its declared type is coerced the way SQLLogicTest coerces it - which
//! is why the generator records the type letters SQLite's own values imply.

/// One record of a test file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// A statement that must succeed or must fail.
    Statement {
        /// Whether it must succeed.
        expect_ok: bool,
        /// The SQL.
        sql: String,
    },
    /// A query and the values it must return.
    Query {
        /// The type letters, one per result column.
        types: String,
        /// The sort mode: `nosort`, `rowsort` or `valuesort`.
        sort: String,
        /// The optional label.
        label: Option<String>,
        /// The SQL.
        sql: String,
        /// The expected values, one per line, in the file's order.
        expected: Vec<String>,
    },
}

/// A parsed test file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TestFile {
    /// The records, in file order.
    pub records: Vec<Record>,
}

impl TestFile {
    /// Parses a SQLLogicTest file.
    pub fn parse(text: &str) -> Result<TestFile, String> {
        let lines: Vec<&str> = text.lines().collect();
        let mut records = Vec::new();
        let mut index = 0usize;
        while index < lines.len() {
            let line = lines.get(index).copied().unwrap_or("").trim_end();
            index = index.saturating_add(1);
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            let keyword = words.next().unwrap_or("");
            match keyword {
                "statement" => {
                    let expect_ok = words.next() == Some("ok");
                    let (sql, next) = take_sql(&lines, index);
                    index = next;
                    records.push(Record::Statement { expect_ok, sql });
                }
                "query" => {
                    let types = words.next().unwrap_or("").to_string();
                    // The type field is letters and nothing else. A header
                    // written with no letters at all collapses on the split and
                    // reads its sort mode as the types, which is a file that
                    // silently means something different from what it says.
                    if types.is_empty()
                        || !types
                            .chars()
                            .all(|letter| matches!(letter, 'T' | 'I' | 'R'))
                    {
                        return Err(format!("a query record with type letters `{types}`"));
                    }
                    let sort = words.next().unwrap_or("nosort").to_string();
                    let label = words.next().map(str::to_string);
                    let (sql, next) = take_sql(&lines, index);
                    index = next;
                    let mut expected = Vec::new();
                    // A `----` separator introduces the expected values, which
                    // run to the next blank line.
                    if lines.get(index).map(|line| line.trim()) == Some("----") {
                        index = index.saturating_add(1);
                        while let Some(value) = lines.get(index) {
                            if value.trim().is_empty() {
                                break;
                            }
                            expected.push((*value).to_string());
                            index = index.saturating_add(1);
                        }
                    }
                    records.push(Record::Query {
                        types,
                        sort,
                        label,
                        sql,
                        expected,
                    });
                }
                "halt" => break,
                other => {
                    return Err(format!("unsupported record `{other}`"));
                }
            }
        }
        Ok(TestFile { records })
    }

    /// Renders the file back out.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for record in &self.records {
            match record {
                Record::Statement { expect_ok, sql } => {
                    out.push_str(if *expect_ok {
                        "statement ok\n"
                    } else {
                        "statement error\n"
                    });
                    out.push_str(sql);
                    out.push_str("\n\n");
                }
                Record::Query {
                    types,
                    sort,
                    label,
                    sql,
                    expected,
                } => {
                    out.push_str(&format!("query {types} {sort}"));
                    if let Some(label) = label {
                        out.push(' ');
                        out.push_str(label);
                    }
                    out.push('\n');
                    out.push_str(sql);
                    out.push_str("\n----\n");
                    for value in expected {
                        out.push_str(value);
                        out.push('\n');
                    }
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// Reads the SQL that follows a record header, up to a blank line or `----`.
fn take_sql(lines: &[&str], from: usize) -> (String, usize) {
    let mut sql = String::new();
    let mut index = from;
    while let Some(line) = lines.get(index) {
        if line.trim().is_empty() || line.trim() == "----" {
            break;
        }
        if !sql.is_empty() {
            sql.push('\n');
        }
        sql.push_str(line);
        index = index.saturating_add(1);
    }
    (sql, index)
}

/// Renders one value the way SQLLogicTest writes it.
///
/// The three special forms are the format's own: a null is `NULL`, an empty
/// string is `(empty)`, and a string that is otherwise empty of printable
/// characters keeps its bytes. Everything else is the value's text.
pub fn render_value(value: &inillucent_value::Value<'_>, letter: char) -> String {
    use inillucent_value::Value;
    let text = match value {
        Value::Null => return "NULL".to_string(),
        // A number is rendered as the declared letter asks, because that is
        // where the letter carries information: an integer under `R` is a real.
        Value::Integer(integer) => match letter {
            'R' => format_real(*integer as f64),
            _ => integer.to_string(),
        },
        Value::Real(real) => match letter {
            'I' => (*real as i64).to_string(),
            _ => format_real(*real),
        },
        // Text and blobs are rendered as themselves whatever the letter says.
        // A schemaless column holds different classes in different rows, so no
        // one letter is right for it, and coercing `abc` to `0` because an
        // earlier row was an integer compares two different things.
        other => text_of(other),
    };
    if text.is_empty() {
        return "(empty)".to_string();
    }
    text
}

/// Returns the text of a value, with a blob rendered as hexadecimal.
fn text_of(value: &inillucent_value::Value<'_>) -> String {
    use inillucent_value::Value;
    match value {
        Value::Null => String::new(),
        Value::Integer(integer) => integer.to_string(),
        Value::Real(real) => format_real(*real),
        Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
        Value::Blob(blob) => blob
            .raw()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<String>>()
            .join(""),
    }
}

/// Renders a real the way SQLLogicTest does: three decimal places.
///
/// The format deliberately loses precision here, and that is the point - a
/// difference in the seventeenth digit of a double is a difference between two
/// correct implementations, not a parity failure.
pub fn format_real(value: f64) -> String {
    format!("{value:.3}")
}

/// Applies a sort mode to a set of rendered rows, returning the flat values.
///
/// `nosort` keeps the engine's order and is used only where the query asked for
/// one with `ORDER BY`. `rowsort` orders the rows, which is what makes a
/// comparison about the *rows* rather than about which access path each engine
/// happened to choose - two engines that both answer correctly can return the
/// same rows in different orders, and only one of them can be "first".
pub fn apply_sort(rows: Vec<Vec<String>>, sort: &str) -> Vec<String> {
    let mut rows = rows;
    match sort {
        "rowsort" => rows.sort(),
        "valuesort" => {
            let mut values: Vec<String> = rows.into_iter().flatten().collect();
            values.sort();
            return values;
        }
        _ => {}
    }
    rows.into_iter().flatten().collect()
}

/// Returns the sort mode a query should be compared with.
///
/// A query with an `ORDER BY` at the top level has asked for an order and is
/// compared in it; anything else has not, and SQL promises nothing about the
/// order it comes back in.
pub fn sort_mode_for(sql: &str) -> &'static str {
    let upper = sql.to_ascii_uppercase();
    let mut depth = 0i32;
    let bytes = upper.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes.get(index) {
            Some(b'(') => depth = depth.saturating_add(1),
            Some(b')') => depth = depth.saturating_sub(1),
            Some(b'O')
                if depth == 0 && upper.get(index..index.saturating_add(8)) == Some("ORDER BY") =>
            {
                return "nosort";
            }
            _ => {}
        }
        index = index.saturating_add(1);
    }
    "rowsort"
}

/// Returns the type letter a value implies.
pub fn type_letter(value: &inillucent_value::Value<'_>) -> char {
    use inillucent_value::Value;
    match value {
        Value::Integer(_) => 'I',
        Value::Real(_) => 'R',
        _ => 'T',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_value::Value;

    /// A file round-trips through parse and render, so a generated file and a
    /// hand-written one are the same thing to the runner.
    #[test]
    fn a_file_round_trips() {
        let text =
            "statement ok\nCREATE TABLE t(a)\n\nquery I nosort\nSELECT a FROM t\n----\n1\nNULL\n\n";
        let parsed = TestFile::parse(text).expect("it parses");
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.render(), text);
    }

    /// The three special value forms are the ones the format defines.
    #[test]
    fn the_special_value_forms_are_rendered() {
        assert_eq!(render_value(&Value::Null, 'T'), "NULL");
        assert_eq!(
            render_value(&Value::owned_text(b"").expect("owned"), 'T'),
            "(empty)"
        );
        assert_eq!(render_value(&Value::Integer(-3), 'I'), "-3");
        assert_eq!(render_value(&Value::Real(1.0), 'R'), "1.000");
    }

    /// A real is compared at three decimal places, so two correct engines that
    /// differ in the last bit still agree.
    #[test]
    fn reals_are_compared_at_three_places() {
        assert_eq!(format_real(0.1 + 0.2), format_real(0.3));
        assert_ne!(format_real(1.0), format_real(1.01));
    }

    /// A header with no type letters is refused: it renders as two spaces and
    /// reads back with the sort mode in the types field, which is a file that
    /// silently means something else.
    #[test]
    fn a_query_with_no_type_letters_is_refused() {
        assert!(TestFile::parse(
            "query  rowsort
SELECT 1
----

"
        )
        .is_err());
    }

    /// A query with a top-level ORDER BY is compared in order; one without is
    /// compared as a set of rows, because SQL promises nothing about the order.
    #[test]
    fn the_sort_mode_follows_the_order_by() {
        assert_eq!(sort_mode_for("SELECT a FROM t"), "rowsort");
        assert_eq!(sort_mode_for("SELECT a FROM t ORDER BY a"), "nosort");
        assert_eq!(
            sort_mode_for("SELECT a FROM t WHERE b IN (SELECT c FROM u ORDER BY c)"),
            "rowsort"
        );
    }

    /// `rowsort` orders whole rows rather than loose values, so a row does not
    /// get its columns shuffled between other rows.
    #[test]
    fn rowsort_keeps_rows_together() {
        let rows = vec![
            vec!["2".to_string(), "b".to_string()],
            vec!["1".to_string(), "a".to_string()],
        ];
        assert_eq!(
            apply_sort(rows, "rowsort"),
            vec![
                "1".to_string(),
                "a".to_string(),
                "2".to_string(),
                "b".to_string()
            ]
        );
    }

    /// An unknown record is refused rather than skipped, because a skipped
    /// record is a test that silently stopped checking anything.
    #[test]
    fn an_unknown_record_is_refused() {
        assert!(TestFile::parse("skipif mssql\nSELECT 1\n").is_err());
    }
}
