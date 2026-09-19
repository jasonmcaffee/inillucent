//! The output modes: how a row becomes the text a person or a script reads.
//!
//! Invariant: a mode decides layout and nothing else. Every mode is handed the
//! same values, and none of them converts one - a blob prints as its bytes in
//! `list` and as an `x'...'` literal in `quote` because those are two ways of
//! writing the same value, not two values. The conversions themselves belong to
//! the engine, which is why nothing in this file parses or casts anything.
//!
//! The default is SQLite's: `list` mode, `|` between columns, headers off, and
//! NULL as the empty string. That last one is a genuinely bad default and it is
//! kept anyway, because a script written against `sqlite3` and pointed at this
//! shell has to see the same bytes.

use inillucent_value::Value;

/// How rows are laid out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Columns separated by the separator, one row per line.
    #[default]
    List,
    /// Fixed-width columns, padded to the widest value.
    Column,
    /// One `name = value` line per column, a blank line between rows.
    Line,
    /// Comma-separated, quoted the way a spreadsheet expects.
    Csv,
    /// Columns separated by tabs.
    Tabs,
    /// Every value as an SQL literal.
    Quote,
    /// One `INSERT INTO` statement per row.
    Insert,
    /// A JSON array of objects.
    Json,
    /// A Markdown table.
    Markdown,
    /// A table drawn with `+` and `-`.
    Table,
    /// A table drawn with box-drawing characters.
    Box,
    /// An HTML table body.
    Html,
}

impl Mode {
    /// Returns the mode a name selects.
    pub fn from_name(name: &str) -> Option<Mode> {
        match name.to_ascii_lowercase().as_str() {
            "list" => Some(Mode::List),
            "column" | "columns" => Some(Mode::Column),
            "line" | "lines" => Some(Mode::Line),
            "csv" => Some(Mode::Csv),
            "tabs" => Some(Mode::Tabs),
            "quote" => Some(Mode::Quote),
            "insert" => Some(Mode::Insert),
            "json" => Some(Mode::Json),
            "markdown" => Some(Mode::Markdown),
            "table" => Some(Mode::Table),
            "box" => Some(Mode::Box),
            "html" => Some(Mode::Html),
            _ => None,
        }
    }

    /// Returns the column separator this mode starts with.
    ///
    /// Choosing a mode resets the separator, which is why `.mode csv` produces
    /// commas without being told to. A `.separator` afterwards still wins.
    pub fn separator(self) -> &'static str {
        match self {
            Mode::Csv | Mode::Quote => ",",
            Mode::Tabs => "\t",
            _ => "|",
        }
    }

    /// Returns the name `.show` prints for this mode.
    pub fn name(self) -> &'static str {
        match self {
            Mode::List => "list",
            Mode::Column => "column",
            Mode::Line => "line",
            Mode::Csv => "csv",
            Mode::Tabs => "tabs",
            Mode::Quote => "quote",
            Mode::Insert => "insert",
            Mode::Json => "json",
            Mode::Markdown => "markdown",
            Mode::Table => "table",
            Mode::Box => "box",
            Mode::Html => "html",
        }
    }
}

/// Everything a mode needs that is not the rows themselves.
#[derive(Clone, Debug)]
pub struct Layout {
    /// Which mode.
    pub mode: Mode,
    /// What goes between columns, in the modes that use one.
    pub separator: String,
    /// What goes between rows.
    pub row_separator: String,
    /// What a NULL prints as.
    pub null: String,
    /// Whether to print a header line.
    pub headers: bool,
    /// The table name `insert` mode writes.
    pub table: String,
    /// The column widths `.width` fixed, if any.
    pub widths: Vec<usize>,
}

impl Default for Layout {
    /// Returns SQLite's defaults.
    fn default() -> Layout {
        Layout {
            mode: Mode::List,
            separator: "|".to_string(),
            row_separator: "\n".to_string(),
            null: String::new(),
            headers: false,
            // "tab", not "table": the reference chose a name that is not
            // a keyword, so the statements it writes can be pasted back.
            table: "tab".to_string(),
            widths: Vec::new(),
        }
    }
}

/// Renders a result set into the lines that should be printed.
///
/// It returns lines rather than writing, so the caller decides where they go -
/// which is what `.output` and `.once` need, and what makes this testable
/// without a file.
pub fn render(layout: &Layout, columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<String> {
    match layout.mode {
        Mode::List | Mode::Tabs => separated(layout, columns, rows),
        Mode::Csv => csv(layout, columns, rows),
        Mode::Quote => quoted(layout, columns, rows),
        Mode::Line => lines(layout, columns, rows),
        Mode::Insert => inserts(layout, columns, rows),
        Mode::Json => json(layout, columns, rows),
        Mode::Column => aligned(layout, columns, rows),
        Mode::Markdown | Mode::Table | Mode::Box => drawn(layout, columns, rows),
        Mode::Html => html(layout, columns, rows),
    }
}

/// Returns the text a value prints as in the plain modes.
fn plain(layout: &Layout, value: &Value<'static>) -> String {
    match value {
        Value::Null => layout.null.clone(),
        Value::Integer(number) => number.to_string(),
        Value::Real(_) => number_text(value),
        Value::Text(text) => printable(text.raw()),
        Value::Blob(blob) => printable(blob.raw()),
    }
}

/// Returns bytes as the shell prints them.
///
/// Two rules, both the reference's. A value is printed as a C string, so it
/// stops at the first NUL; and a control character is printed in caret
/// notation, because a shell that emitted raw control bytes could be made to
/// drive a terminal by the contents of a database. A newline is left alone,
/// since a multi-line value is meant to look like one.
fn printable(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let visible = bytes.get(..end).unwrap_or(bytes);
    let mut out = String::with_capacity(visible.len());
    for chunk in String::from_utf8_lossy(visible).chars() {
        let code = chunk as u32;
        if chunk == '\n' {
            out.push(chunk);
        } else if code < 0x20 {
            out.push('^');
            out.push(char::from_u32(code + 0x40).unwrap_or('?'));
        } else if code == 0x7f {
            out.push_str("^?");
        } else {
            out.push(chunk);
        }
    }
    out
}

/// Returns the text the engine writes a number as.
fn number_text(value: &Value<'static>) -> String {
    let cast = inillucent_value::cast::cast_value(
        value.clone(),
        inillucent_value::Affinity::Text,
        inillucent_value::TextEncoding::Utf8,
    );
    match cast {
        Ok(Value::Text(text)) => String::from_utf8_lossy(text.raw()).into_owned(),
        _ => String::new(),
    }
}

/// Returns the SQL literal a value would be written as.
pub fn literal(value: &Value<'static>) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(number) => number.to_string(),
        Value::Real(_) => number_text(value),
        Value::Text(text) => {
            let body = String::from_utf8_lossy(text.raw()).replace('\'', "''");
            format!("'{body}'")
        }
        Value::Blob(blob) => {
            // Lower case, both the `x` and the digits: it is what the reference
            // writes, and a dump is compared against one.
            let mut out = String::from("x'");
            for byte in blob.raw() {
                out.push_str(&format!("{byte:02x}"));
            }
            out.push('\'');
            out
        }
    }
}

/// `list` and `tabs`: values with a separator between them.
fn separated(layout: &Layout, columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<String> {
    let separator = if layout.mode == Mode::Tabs {
        "\t"
    } else {
        layout.separator.as_str()
    };
    let mut out = Vec::with_capacity(rows.len() + 1);
    if layout.headers {
        out.push(columns.join(separator));
    }
    for row in rows {
        let cells: Vec<String> = row.iter().map(|value| plain(layout, value)).collect();
        out.push(cells.join(separator));
    }
    out
}

/// `csv`: a field is quoted when it holds a comma, a quote or a newline.
fn csv(layout: &Layout, columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<String> {
    let mut out = Vec::with_capacity(rows.len() + 1);
    if layout.headers {
        out.push(
            columns
                .iter()
                .map(|name| csv_field(name))
                .collect::<Vec<String>>()
                .join(","),
        );
    }
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|value| csv_field(&plain(layout, value)))
            .collect();
        out.push(cells.join(","));
    }
    // The caller writes a newline after each line, so a row separator of
    // CR LF is a carriage return on the end of the line itself.
    //
    // **And a second one on Windows**, which looks wrong and is not. The
    // reference's row separator is CR LF, and it writes it through a text-mode
    // C stream that translates the LF into CR LF on the way out - so the bytes
    // a caller actually receives from `sqlite3 -csv` on this platform are
    // **CR CR LF**, and a shell that emitted the two-byte sequence would not be
    // byte-compatible with the thing it is replacing. Rust's `println!` does no
    // such translation, so the translation is done here, where it can be
    // labelled. Elsewhere the reference's own stream emits CR LF and so do we.
    if layout.row_separator.ends_with(CRLF) {
        for line in &mut out {
            line.push(CR);
            if cfg!(windows) {
                line.push(CR);
            }
        }
    }
    out
}

/// The row separator RFC 4180 gives a CSV record, and the reference writes.
const CRLF: &str = "\r\n";

/// The carriage return half of it.
const CR: char = '\r';

/// Quotes one CSV field, if it needs it.
///
/// The separator, a quote and a line break all force quoting, and so does a
/// control character - it has already been turned into caret notation by the
/// time this sees it, and quoting is how the reference marks that the field was
/// not plain text to begin with.
fn csv_field(text: &str) -> String {
    let needs = text.contains(',')
        || text.contains('"')
        || text.contains('\n')
        || text.contains('\r')
        || text.contains('^');
    if !needs {
        return text.to_string();
    }
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// `quote`: every value as the literal it would be written as.
fn quoted(layout: &Layout, columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<String> {
    let mut out = Vec::with_capacity(rows.len() + 1);
    if layout.headers {
        out.push(
            columns
                .iter()
                .map(|name| format!("'{}'", name.replace('\'', "''")))
                .collect::<Vec<String>>()
                .join(&layout.separator),
        );
    }
    for row in rows {
        let cells: Vec<String> = row.iter().map(literal).collect();
        out.push(cells.join(&layout.separator));
    }
    out
}

/// `line`: one `name: value` per column, rows separated by a blank line.
fn lines(layout: &Layout, columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<String> {
    let width = columns
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            out.push(String::new());
        }
        for (position, value) in row.iter().enumerate() {
            let name = columns.get(position).cloned().unwrap_or_default();
            out.push(format!("{name:>width$}: {}", plain(layout, value)));
        }
    }
    out
}

/// `insert`: one statement per row, which is what a dump is made of.
fn inserts(layout: &Layout, columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<String> {
    let _ = columns;
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row.iter().map(literal).collect();
            format!("INSERT INTO {} VALUES({});", layout.table, cells.join(","))
        })
        .collect()
}

/// `json`: an array of objects, one per row.
fn json(layout: &Layout, columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<String> {
    let _ = layout;
    if rows.is_empty() {
        // An empty result prints nothing, not an empty array: the reference
        // writes the brackets around rows and there are none.
        return Vec::new();
    }
    let mut out = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let members: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(position, value)| {
                let name = columns.get(position).cloned().unwrap_or_default();
                format!(
                    "\"{}\":{}",
                    inillucent_base::json::escape(&name),
                    json_value(value)
                )
            })
            .collect();
        let open = if index == 0 { "[" } else { "" };
        let close = if index + 1 == rows.len() { "]" } else { "," };
        out.push(format!("{open}{{{}}}{close}", members.join(",")));
    }
    out
}

/// Returns a value as JSON.
fn json_value(value: &Value<'static>) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Integer(number) => number.to_string(),
        Value::Real(_) => number_text(value),
        Value::Text(text) => format!(
            "\"{}\"",
            inillucent_base::json::escape(&String::from_utf8_lossy(text.raw()))
        ),
        // A blob's bytes, each as its own escape: the reference writes
        // `"\u00ab"` rather than the hex a reader might expect, and a consumer
        // of the JSON is reading whichever one it was given.
        Value::Blob(blob) => {
            let escaped: String = blob
                .raw()
                .iter()
                .map(|byte| format!("\\u{byte:04x}"))
                .collect();
            format!("\"{escaped}\"")
        }
    }
}

/// Returns each column's width: the widest of its values and its name.
fn widths(layout: &Layout, columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<usize> {
    let mut widths: Vec<usize> = columns.iter().map(|name| name.chars().count()).collect();
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            let width = plain(layout, value).chars().count();
            match widths.get_mut(index) {
                Some(existing) => *existing = (*existing).max(width),
                None => widths.push(width),
            }
        }
    }
    for (index, fixed) in layout.widths.iter().enumerate() {
        if *fixed == 0 {
            continue;
        }
        if let Some(existing) = widths.get_mut(index) {
            *existing = *fixed;
        }
    }
    widths
}

/// `column`: fixed-width columns with two spaces between them.
fn aligned(layout: &Layout, columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<String> {
    let widths = widths(layout, columns, rows);
    let mut out = Vec::with_capacity(rows.len() + 2);
    if layout.headers {
        let centred: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(index, name)| centre(name, widths.get(index).copied().unwrap_or(0)))
            .collect();
        out.push(centred.join("  ").trim_end().to_string());
        out.push(
            widths
                .iter()
                .map(|width| "-".repeat(*width))
                .collect::<Vec<String>>()
                .join("  "),
        );
    }
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(index, value)| align(layout, value, widths.get(index).copied().unwrap_or(0)))
            .collect();
        out.push(pad_row(&cells, &widths));
    }
    out
}

/// Returns one cell padded to its width, right-aligned when it is a number.
///
/// A number is right-aligned and everything else is not, which is what makes a
/// column of amounts line up on its digits.
fn align(layout: &Layout, value: &Value<'static>, width: usize) -> String {
    let text = plain(layout, value);
    if matches!(value, Value::Integer(_) | Value::Real(_)) {
        return format!("{text:>width$}");
    }
    text
}

/// Centres text in a field, leaning left when it cannot be even.
///
/// Every tabular mode centres its headers over left-aligned values, which is
/// the reference's choice and looks better than it sounds: a narrow numeric
/// column under a long name is unreadable left-aligned.
fn centre(text: &str, width: usize) -> String {
    let length = text.chars().count();
    if length >= width {
        return text.to_string();
    }
    let left = (width - length) / 2;
    let right = width - length - left;
    format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
}

/// Pads a row of cells to the given widths, trimming the trailing run.
fn pad_row(cells: &[String], widths: &[usize]) -> String {
    let padded: Vec<String> = cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let width = widths.get(index).copied().unwrap_or(0);
            format!("{cell:<width$}")
        })
        .collect();
    padded.join("  ").trim_end().to_string()
}

/// The characters one drawn table is made of.
struct Frame {
    left: &'static str,
    middle: &'static str,
    right: &'static str,
    horizontal: &'static str,
    vertical: &'static str,
}

/// `markdown`, `table` and `box`: a header, a rule, and the rows.
fn drawn(layout: &Layout, columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<String> {
    let widths = widths(layout, columns, rows);
    let frame = match layout.mode {
        Mode::Markdown => Frame {
            left: "|",
            middle: "|",
            right: "|",
            horizontal: "-",
            vertical: "|",
        },
        // The reference draws its box with rounded corners and a double
        // rule under the header. That is copied exactly rather than
        // approximated: the whole value of the mode is that a person
        // recognises the output.
        Mode::Box => Frame {
            left: "\u{256d}",
            middle: "\u{252c}",
            right: "\u{256e}",
            horizontal: "\u{2500}",
            vertical: "\u{2502}",
        },
        _ => Frame {
            left: "+",
            middle: "+",
            right: "+",
            horizontal: "-",
            vertical: "|",
        },
    };
    let mut out = Vec::with_capacity(rows.len() + 4);
    let rule = rule_line(&frame, &widths);
    if layout.mode != Mode::Markdown {
        out.push(rule.clone());
    }
    let centred: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(index, name)| centre(name, widths.get(index).copied().unwrap_or(0)))
        .collect();
    out.push(drawn_row(&frame, &centred, &widths, layout, true));
    out.push(match layout.mode {
        Mode::Markdown => markdown_rule(&widths),
        Mode::Box => rule_line(
            &Frame {
                left: "\u{255e}",
                middle: "\u{256a}",
                right: "\u{2561}",
                horizontal: "\u{2550}",
                ..frame
            },
            &widths,
        ),
        _ => rule.clone(),
    });
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(index, value)| align(layout, value, widths.get(index).copied().unwrap_or(0)))
            .collect();
        out.push(drawn_row(&frame, &cells, &widths, layout, false));
    }
    if layout.mode == Mode::Box {
        out.push(rule_line(
            &Frame {
                left: "\u{2570}",
                middle: "\u{2534}",
                right: "\u{256f}",
                ..frame
            },
            &widths,
        ));
    } else if layout.mode != Mode::Markdown {
        out.push(rule);
    }
    out
}

/// Returns one horizontal rule.
fn rule_line(frame: &Frame, widths: &[usize]) -> String {
    let parts: Vec<String> = widths
        .iter()
        .map(|width| frame.horizontal.repeat(width + 2))
        .collect();
    format!("{}{}{}", frame.left, parts.join(frame.middle), frame.right)
}

/// Returns the `|---|---|` line Markdown wants under its header.
fn markdown_rule(widths: &[usize]) -> String {
    let parts: Vec<String> = widths.iter().map(|width| "-".repeat(width + 2)).collect();
    format!("|{}|", parts.join("|"))
}

/// Returns one drawn row, padded to the widths.
fn drawn_row(
    frame: &Frame,
    cells: &[String],
    widths: &[usize],
    layout: &Layout,
    header: bool,
) -> String {
    let _ = (layout, header);
    let padded: Vec<String> = widths
        .iter()
        .enumerate()
        .map(|(index, width)| {
            let cell = cells.get(index).cloned().unwrap_or_default();
            format!(" {cell:<width$} ")
        })
        .collect();
    format!(
        "{}{}{}",
        frame.vertical,
        padded.join(frame.vertical),
        frame.vertical
    )
}

/// `html`: a table body, which is what a caller pastes into a page.
///
/// One cell per line and no closing cell tags, which is what the reference
/// emits. It is valid HTML - a `<TD>` closes the one before it - and it is what
/// a diff against the reference has to produce.
fn html(layout: &Layout, columns: &[String], rows: &[Vec<Value<'static>>]) -> Vec<String> {
    let mut out = Vec::new();
    if layout.headers {
        out.push("<TR>".to_string());
        for name in columns {
            out.push(format!("<TH>{}", html_escape(name)));
        }
        out.push("</TR>".to_string());
    }
    for row in rows {
        out.push("<TR>".to_string());
        for value in row {
            out.push(format!("<TD>{}", html_escape(&plain(layout, value))));
        }
        out.push("</TR>".to_string());
    }
    out
}

/// Escapes the four characters that mean something in HTML.
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a one-row result for the tests below.
    fn sample() -> (Vec<String>, Vec<Vec<Value<'static>>>) {
        let columns = vec!["a".to_string(), "b".to_string()];
        let rows = vec![vec![
            Value::Integer(1),
            Value::owned_text(b"two").expect("owns"),
        ]];
        (columns, rows)
    }

    /// The default mode is SQLite's: pipes, no headers.
    #[test]
    fn the_default_is_a_pipe_separated_line() {
        let (columns, rows) = sample();
        let out = render(&Layout::default(), &columns, &rows);
        assert_eq!(out, vec!["1|two"]);
    }

    /// Headers are the column names, in the same layout as the rows.
    #[test]
    fn headers_use_the_same_layout() {
        let (columns, rows) = sample();
        let layout = Layout {
            headers: true,
            ..Layout::default()
        };
        let out = render(&layout, &columns, &rows);
        assert_eq!(out, vec!["a|b", "1|two"]);
    }

    /// A CSV field is quoted only when it has to be.
    #[test]
    fn csv_quotes_only_what_it_must() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    /// A control character is escaped and a NUL ends the value.
    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(printable(b"ab"), "ab");
        assert_eq!(printable(&[0x01, 0x02]), "^A^B");
        assert_eq!(printable(&[0x09, b't']), "^It");
        assert_eq!(printable(&[0x7f]), "^?");
        assert_eq!(printable(b"a\nb"), "a\nb");
        assert_eq!(printable(&[b'a', 0, b'b']), "a");
    }

    /// Quote mode writes values as SQL literals, blobs included.
    #[test]
    fn quote_mode_writes_literals() {
        let columns = vec!["x".to_string()];
        let rows = vec![
            vec![Value::Null],
            vec![Value::owned_blob(&[1, 255]).expect("owns")],
            vec![Value::owned_text(b"it's").expect("owns")],
        ];
        let layout = Layout {
            mode: Mode::Quote,
            ..Layout::default()
        };
        let out = render(&layout, &columns, &rows);
        assert_eq!(out, vec!["NULL", "x'01ff'", "'it''s'"]);
    }

    /// Every mode name round-trips.
    #[test]
    fn every_mode_name_round_trips() {
        for mode in [
            Mode::List,
            Mode::Column,
            Mode::Line,
            Mode::Csv,
            Mode::Tabs,
            Mode::Quote,
            Mode::Insert,
            Mode::Json,
            Mode::Markdown,
            Mode::Table,
            Mode::Box,
            Mode::Html,
        ] {
            assert_eq!(Mode::from_name(mode.name()), Some(mode), "{}", mode.name());
        }
        assert_eq!(Mode::from_name("nonsense"), None);
    }

    /// A drawn table has a rule above and below its rows.
    #[test]
    fn a_table_is_drawn_with_rules() {
        let (columns, rows) = sample();
        let layout = Layout {
            mode: Mode::Table,
            headers: true,
            ..Layout::default()
        };
        let out = render(&layout, &columns, &rows);
        assert_eq!(out.len(), 5, "{out:#?}");
        assert!(out.first().is_some_and(|line| line.starts_with('+')));
        assert!(out.last().is_some_and(|line| line.starts_with('+')));
    }

    /// JSON escapes what JSON has to escape.
    #[test]
    fn the_shell_escapes_through_the_base_crate() {
        assert_eq!(inillucent_base::json::escape("a\"b"), "a\\\"b");
        assert_eq!(inillucent_base::json::escape("a\nb"), "a\\nb");
        assert_eq!(inillucent_base::json::escape("a\u{1}b"), "a\\u0001b");
        // The two the private copy spelled as `\u0008` and `\u000c`, and the
        // one it left unescaped.
        assert_eq!(inillucent_base::json::escape("a\u{8}b"), "a\\bb");
        assert_eq!(inillucent_base::json::escape("a\u{c}b"), "a\\fb");
        assert_eq!(inillucent_base::json::escape("a\u{7f}b"), "a\\u007fb");
    }
}
