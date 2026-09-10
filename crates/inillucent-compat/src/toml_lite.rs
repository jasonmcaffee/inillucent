//! A strict reader for the manifest subset of TOML.
//!
//! Invariant: anything the reader does not understand is an error, never a
//! silent skip. A manifest row that is quietly ignored takes a capability out
//! of the compatibility report without anyone noticing, which is the one
//! failure mode a parity manifest cannot tolerate.
//!
//! The subset is deliberately tiny: top-level `key = value` scalars, `[[array]]`
//! tables of scalars, and arrays of strings. A general TOML crate would accept
//! far more shapes than the manifests use, and the manifests are a contract
//! rather than a configuration file.

use std::collections::BTreeMap;

/// A scalar value a manifest may hold.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A quoted string.
    Text(String),
    /// An integer.
    Integer(i64),
    /// A boolean.
    Boolean(bool),
    /// An array of quoted strings.
    List(Vec<String>),
}

impl Value {
    /// Returns the string body, or `None` when the value is another type.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    /// Returns the integer body, or `None` when the value is another type.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(value) => Some(*value),
            _ => None,
        }
    }

    /// Returns the boolean body, or `None` when the value is another type.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(value) => Some(*value),
            _ => None,
        }
    }

    /// Returns the list body, or `None` when the value is another type.
    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }
}

/// One `[[array]]` table.
pub type Table = BTreeMap<String, Value>;

/// A parsed manifest.
#[derive(Clone, Debug, Default)]
pub struct Document {
    /// The scalars written before any `[[array]]` header.
    pub top: Table,
    /// The `[[array]]` tables, in file order, by array name.
    pub arrays: BTreeMap<String, Vec<Table>>,
    /// The plain `[table]` sections, by name.
    ///
    /// **One section, not a list.** A second `[memory]` header merges into the
    /// first rather than making a second table, which is what TOML itself says
    /// happens and is the only behaviour a reader of this subset could sensibly
    /// expect. The subset refused plain tables outright until the performance
    /// contract needed `[memory]` and `[cpu]`.
    pub tables: BTreeMap<String, Table>,
}

impl Document {
    /// Returns the tables of one array, or an empty slice.
    pub fn array(&self, name: &str) -> &[Table] {
        self.arrays
            .get(name)
            .map(|rows| rows.as_slice())
            .unwrap_or(&[])
    }

    /// Returns one plain `[table]` section, or an empty one.
    ///
    /// @param name - the section's name, without its brackets
    pub fn table(&self, name: &str) -> Table {
        self.tables.get(name).cloned().unwrap_or_default()
    }

    /// Returns a required top-level string, or an error naming what is missing.
    pub fn require_str(&self, key: &str) -> Result<&str, String> {
        self.top
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("missing top-level string `{key}`"))
    }
}

/// Parses the manifest subset, reporting the line number of anything it does
/// not understand.
pub fn parse(text: &str) -> Result<Document, String> {
    let mut document = Document::default();
    let mut current: Option<String> = None;
    let mut section: Option<String> = None;
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0usize;
    while index < lines.len() {
        let raw = lines.get(index).copied().unwrap_or("");
        let number = index + 1;
        index += 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line
            .strip_prefix("[[")
            .and_then(|rest| rest.strip_suffix("]]"))
        {
            let name = name.trim().to_string();
            document
                .arrays
                .entry(name.clone())
                .or_default()
                .push(Table::new());
            current = Some(name);
            section = None;
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            let name = name.trim().to_string();
            document.tables.entry(name.clone()).or_default();
            current = None;
            section = Some(name);
            continue;
        }
        if line.starts_with('[') {
            return Err(format!("line {number}: expected a `[table]` header"));
        }
        let Some((key, body)) = line.split_once('=') else {
            return Err(format!("line {number}: expected `key = value`"));
        };
        // An array may be written across several lines. Gather them until the
        // brackets balance, so a long `may_depend_on` list can be readable
        // without the reader silently taking only its first line.
        let mut body = body.trim().to_string();
        if body.starts_with('[') {
            while !balanced(&body) {
                let Some(next) = lines.get(index) else {
                    return Err(format!("line {number}: unterminated array `{body}`"));
                };
                index += 1;
                body.push(' ');
                body.push_str(strip_comment(next).trim());
            }
        }
        let value =
            parse_value(body.trim()).map_err(|reason| format!("line {number}: {reason}"))?;
        let key = key.trim().to_string();
        match current.as_ref() {
            None => match section.as_ref() {
                None => {
                    document.top.insert(key, value);
                }
                Some(name) => {
                    let Some(row) = document.tables.get_mut(name) else {
                        return Err(format!("line {number}: table `{name}` vanished"));
                    };
                    row.insert(key, value);
                }
            },
            Some(name) => {
                let Some(rows) = document.arrays.get_mut(name) else {
                    return Err(format!("line {number}: array `{name}` vanished"));
                };
                let Some(row) = rows.last_mut() else {
                    return Err(format!("line {number}: array `{name}` has no table"));
                };
                row.insert(key, value);
            }
        }
    }
    Ok(document)
}

/// Reports whether an array body has as many closing brackets as opening ones,
/// ignoring brackets inside strings.
fn balanced(body: &str) -> bool {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for character in body.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '[' if !in_string => depth += 1,
            ']' if !in_string => depth -= 1,
            _ => {}
        }
    }
    depth == 0
}

/// Removes a trailing comment, respecting quoted strings so that a `#` inside a
/// URL or a message is not treated as one.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return line.get(..index).unwrap_or(""),
            _ => {}
        }
    }
    line
}

/// Parses one scalar or string array.
fn parse_value(body: &str) -> Result<Value, String> {
    if body == "true" {
        return Ok(Value::Boolean(true));
    }
    if body == "false" {
        return Ok(Value::Boolean(false));
    }
    if body.starts_with('[') {
        return parse_list(body);
    }
    if body.starts_with('"') {
        return parse_string(body).map(Value::Text);
    }
    body.parse::<i64>()
        .map(Value::Integer)
        .map_err(|_| format!("unsupported literal `{body}`"))
}

/// Parses a double-quoted string with the escapes the manifests use.
fn parse_string(body: &str) -> Result<String, String> {
    let Some(inner) = body
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return Err(format!("unterminated string `{body}`"));
    };
    let mut out = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => return Err(format!("unsupported escape `\\{other}`")),
            None => return Err("string ends in a backslash".to_string()),
        }
    }
    Ok(out)
}

/// Parses a single-line array of quoted strings.
fn parse_list(body: &str) -> Result<Value, String> {
    let Some(inner) = body
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return Err(format!("unterminated array `{body}`"));
    };
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return Ok(Value::List(Vec::new()));
    }
    let mut items = Vec::new();
    for piece in split_top_level(trimmed) {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        items.push(parse_string(piece)?);
    }
    Ok(Value::List(items))
}

/// Splits an array body on commas that are not inside a string.
fn split_top_level(body: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            ',' if !in_string => {
                pieces.push(body.get(start..index).unwrap_or(""));
                start = index.saturating_add(1);
            }
            _ => {}
        }
    }
    pieces.push(body.get(start..).unwrap_or(""));
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shapes the manifests really use must parse, in order.
    #[test]
    fn the_manifest_subset_parses() {
        let text = "\
schema_version = 1
reference = \"sqlite-3.53.4\"

[[capability]]
id = \"sql.select.window.exclude\"
status = \"pass\"
tests = [\"compat/window/exclude.sqltest\", \"upstream/window8.test\"]
released = true

[[capability]]
id = \"sql.grant\"
status = \"missing\"
tests = []
";
        let document = parse(text).expect("the subset parses");
        assert_eq!(document.top.get("schema_version"), Some(&Value::Integer(1)));
        assert_eq!(document.require_str("reference").unwrap(), "sqlite-3.53.4");
        let rows = document.array("capability");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].get("id").and_then(Value::as_str),
            Some("sql.select.window.exclude")
        );
        assert_eq!(
            rows[0]
                .get("tests")
                .and_then(Value::as_list)
                .map(|list| list.len()),
            Some(2)
        );
        assert_eq!(rows[0].get("released").and_then(Value::as_bool), Some(true));
        assert_eq!(rows[1].get("tests").and_then(Value::as_list), Some(&[][..]));
    }

    /// A `#` inside a URL is part of the value, not the start of a comment.
    #[test]
    fn a_hash_inside_a_string_is_not_a_comment() {
        let document = parse("source = \"https://sqlite.org/lang.html#select\" # a real comment")
            .expect("the line parses");
        assert_eq!(
            document.require_str("source").unwrap(),
            "https://sqlite.org/lang.html#select"
        );
    }

    /// A long array may be written across lines, which the layering contract
    /// needs to stay readable.
    #[test]
    fn a_multi_line_array_parses() {
        let document =
            parse("[[crate]]\nname = \"a\"\nmay_depend_on = [\n  \"b\",\n  \"c\",\n]\nlayer = 3\n")
                .expect("the array parses");
        let row = document.array("crate").first().expect("one row");
        assert_eq!(
            row.get("may_depend_on").and_then(Value::as_list),
            Some(&["b".to_string(), "c".to_string()][..])
        );
        assert_eq!(row.get("layer").and_then(Value::as_integer), Some(3));
    }

    /// A plain table parses, which the subset refused until the performance
    /// contract's `[memory]` and `[cpu]` sections needed one.
    ///
    /// It is here because the refusal test below still asserted the old
    /// behaviour after the parser gained it, so `cargo test -p
    /// inillucent-compat` failed on its own lib target on every build - and
    /// because cargo stops at the first failing target, that one stale line
    /// was hiding the whole integration suite.
    #[test]
    fn a_plain_table_parses() {
        let document = parse(
            "[memory]
bar = \"0.95\"
",
        )
        .expect("the table parses");
        assert_eq!(
            document
                .tables
                .get("memory")
                .and_then(|table| table.get("bar"))
                .and_then(Value::as_str),
            Some("0.95")
        );
    }

    /// Anything outside the subset must be refused with a line number, not
    /// skipped, so a mistyped row cannot silently disappear.
    #[test]
    fn anything_outside_the_subset_is_refused() {
        for (text, needle) in [
            ("key\n", "expected `key = value`"),
            ("key = 1.5\n", "unsupported literal"),
            ("key = \"unterminated\n", "unterminated string"),
            ("key = [\"a\",\n", "unterminated array"),
            ("key = \"bad \\q escape\"\n", "unsupported escape"),
        ] {
            let error = parse(text).expect_err("this is outside the subset");
            assert!(error.contains(needle), "{error} did not mention {needle}");
        }
    }
}
