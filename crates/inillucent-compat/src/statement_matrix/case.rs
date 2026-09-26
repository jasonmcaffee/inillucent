//! The matrix case: its records, its file format, and its stable id.
//!
//! Invariant: **a case is the same thing whether a person wrote it in a file or
//! a template generated it.** Both become a [`Case`] holding a setup and a list
//! of [`Record`]s, and the grader never asks where a case came from. A
//! generated case renders to the file format and parses back to itself, which
//! is how a shrunk failure becomes a retained case that runs on every change.
//!
//! The format is sqllogictest's, which `slt.rs` already reads, plus the
//! directives section 6.1 of `tasks/task-2135-sql-statement-matrix-tdd.md`
//! adds. It is parsed here rather than by extending `slt.rs` because `slt.rs`
//! refuses any record it does not know, and that refusal is what keeps the
//! conformance files honest; loosening it for the matrix would loosen it for
//! them too.
//!
//! ```text
//! arms default small_pool       file or case: run only at these arms
//! oracle none                   file: no SQLite equivalent; queries need a result block
//! capability <row>              file or case: the capability row this exercises
//! setup                         starts a new setup for the cases that follow it
//! case <id>                     starts a case; records before it are its setup
//! statement ok                  the statement must succeed on both engines
//! statement error [status]      it must fail on both, with the same codes
//! query <types> <sort> [label]  graded against the oracle, or the result block below it
//! reopen                        close both databases and open them again
//! ```

use std::fmt::Write as _;

use crate::hash::sha3_256_hex;

/// How a query's rows are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sort {
    /// In the order the engine returned them. The loader accepts it only for a
    /// query whose top level `ORDER BY` has a term for every result column.
    NoSort,
    /// As a sorted list of rows.
    RowSort,
    /// As a sorted list of single values.
    ValueSort,
}

impl Sort {
    /// Parses the word a query header uses.
    ///
    /// @param word - `nosort`, `rowsort` or `valuesort`
    pub fn parse(word: &str) -> Option<Sort> {
        match word {
            "nosort" => Some(Sort::NoSort),
            "rowsort" => Some(Sort::RowSort),
            "valuesort" => Some(Sort::ValueSort),
            _ => None,
        }
    }

    /// Returns the word a query header uses.
    pub fn word(self) -> &'static str {
        match self {
            Sort::NoSort => "nosort",
            Sort::RowSort => "rowsort",
            Sort::ValueSort => "valuesort",
        }
    }
}

/// What a statement record expects.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Expect {
    /// Success on both engines.
    Ok,
    /// Failure on both engines, optionally with the inillucent status name
    /// this engine must report.
    Error(Option<String>),
}

/// One record of a case.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Record {
    /// A statement whose rows are not compared.
    Statement {
        /// What it must do.
        expect: Expect,
        /// The SQL, one statement.
        sql: String,
    },
    /// A statement whose rows are compared.
    Query {
        /// The sqllogictest type letters. Documentation here: the values are
        /// compared as typed values, never through these letters.
        types: String,
        /// How the rows are compared.
        sort: Sort,
        /// The recorded answer, one value per line, for a case with no oracle.
        expected: Option<Vec<String>>,
        /// The SQL, one statement.
        sql: String,
    },
    /// Close both databases and open them again.
    Reopen,
}

impl Record {
    /// Returns the record's SQL, or nothing for `reopen`.
    pub fn sql(&self) -> Option<&str> {
        match self {
            Record::Statement { sql, .. } | Record::Query { sql, .. } => Some(sql),
            Record::Reopen => None,
        }
    }

    /// A statement record that must succeed.
    ///
    /// @param sql - the statement
    pub fn ok(sql: impl Into<String>) -> Record {
        Record::Statement {
            expect: Expect::Ok,
            sql: sql.into(),
        }
    }

    /// A query record graded against the oracle.
    ///
    /// @param sort - how the rows are compared
    /// @param sql - the statement
    pub fn query(sort: Sort, sql: impl Into<String>) -> Record {
        Record::Query {
            types: "T".to_string(),
            sort,
            expected: None,
            sql: sql.into(),
        }
    }
}

/// One case: a setup, the records under test, and what it may run at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    /// The stable id, which `known.list` and a failure message name.
    pub id: String,
    /// The statement family, which is also the test module that runs it.
    pub family: String,
    /// Where it came from: a file path, or the template that generated it.
    pub origin: String,
    /// The statements that build the database the records run against. Cases
    /// with the same setup share one fixture (section 7 of the design).
    pub setup: Vec<Record>,
    /// The records under test.
    pub records: Vec<Record>,
    /// The arms it may run at; `None` is every arm the tier runs.
    pub arms: Option<Vec<String>>,
    /// Whether the pinned SQLite has the construct.
    pub oracle: bool,
    /// The capability rows the case exercises. An `unsupported` answer where
    /// SQLite succeeds passes only when one of these rows says `no` or
    /// `partial`.
    pub capabilities: Vec<String>,
    /// The properties that must hold, for a generated case.
    pub properties: Vec<crate::statement_matrix::properties::Property>,
}

impl Case {
    /// A case with no records, for a template to fill in.
    ///
    /// @param family - the statement family
    /// @param origin - what produced it
    pub fn new(family: &str, origin: &str) -> Case {
        Case {
            id: String::new(),
            family: family.to_string(),
            origin: origin.to_string(),
            setup: Vec::new(),
            records: Vec::new(),
            arms: None,
            oracle: true,
            capabilities: Vec::new(),
            properties: Vec::new(),
        }
    }

    /// Gives a generated case its stable id: the family and the first twelve
    /// hex digits of the SHA3-256 of its canonical text.
    ///
    /// The canonical text is the rendered case with its whitespace folded, so
    /// reordering a template's code, or re-indenting its SQL, does not rename
    /// the case and orphan its `known.list` line. SHA3-256 rather than the
    /// design's SHA-256 because it is the digest this crate already has; the
    /// property that matters is only that it is stable.
    pub fn assign_id(&mut self) {
        let mut canonical = String::new();
        for record in self.setup.iter().chain(self.records.iter()) {
            render_record(&mut canonical, record);
        }
        for property in &self.properties {
            let _ = write!(canonical, "{property:?}");
        }
        let folded = canonical
            .split_whitespace()
            .collect::<Vec<&str>>()
            .join(" ");
        let digest = sha3_256_hex(folded.as_bytes());
        self.id = format!("{}-{}", self.family, digest.get(..12).unwrap_or(&digest));
    }

    /// Whether the case writes, which decides whether it is reopened and
    /// checked afterwards.
    pub fn writes(&self) -> bool {
        self.records.iter().any(|record| match record {
            Record::Reopen => true,
            Record::Statement { sql, .. } => !is_read_only(sql),
            Record::Query { sql, .. } => !is_read_only(sql),
        })
    }

    /// Renders the case in the file format, with its setup, so it can be saved
    /// and read back.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# {} from {}", self.id, self.origin);
        if !self.oracle {
            out.push_str("oracle none\n");
        }
        if let Some(arms) = &self.arms {
            let _ = writeln!(out, "arms {}", arms.join(" "));
        }
        for capability in &self.capabilities {
            let _ = writeln!(out, "capability {capability}");
        }
        out.push('\n');
        for record in &self.setup {
            render_record(&mut out, record);
        }
        let _ = writeln!(out, "case {}\n", self.id);
        for record in &self.records {
            render_record(&mut out, record);
        }
        out
    }
}

/// Appends one record in the file format.
///
/// @param out - where to write
/// @param record - the record
pub fn render_record(out: &mut String, record: &Record) {
    match record {
        Record::Statement { expect, sql } => {
            match expect {
                Expect::Ok => out.push_str("statement ok\n"),
                Expect::Error(None) => out.push_str("statement error\n"),
                Expect::Error(Some(status)) => {
                    let _ = writeln!(out, "statement error {status}");
                }
            }
            out.push_str(&one_paragraph(sql));
            out.push_str("\n\n");
        }
        Record::Query {
            types,
            sort,
            expected,
            sql,
        } => {
            let _ = writeln!(out, "query {types} {}", sort.word());
            out.push_str(&one_paragraph(sql));
            out.push('\n');
            if let Some(values) = expected {
                out.push_str("----\n");
                for value in values {
                    out.push_str(value);
                    out.push('\n');
                }
            }
            out.push('\n');
        }
        Record::Reopen => out.push_str("reopen\n\n"),
    }
}

/// Removes blank lines from SQL, because a blank line ends a record.
///
/// @param sql - the statement
fn one_paragraph(sql: &str) -> String {
    sql.lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Whether a statement only reads, by its leading keywords.
///
/// `EXPLAIN` counts as a read, and so does a `PRAGMA` with no `=` and no
/// argument list, which reads a setting back.
///
/// @param sql - the statement
pub fn is_read_only(sql: &str) -> bool {
    use inillucent_sql::StatementClass;
    let trimmed = sql.trim_start();
    let head: String = trimmed
        .chars()
        .take(6)
        .collect::<String>()
        .to_ascii_uppercase();
    if head == "PRAGMA" {
        return !trimmed.contains('=') && !trimmed.contains('(');
    }
    matches!(
        inillucent_sql::classify_statement(sql.as_bytes()),
        StatementClass::ReadOnly
    )
}

/// A parsed case file.
#[derive(Clone, Debug, Default)]
pub struct CaseFile {
    /// The cases, in file order.
    pub cases: Vec<Case>,
}

/// Parses a case file.
///
/// @param text - the file's contents
/// @param family - the family the file belongs to
/// @param origin - the path, for messages and for a case with no `case` line
pub fn parse(text: &str, family: &str, origin: &str) -> Result<CaseFile, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut state = Parse::new(family, origin);
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines.get(index).copied().unwrap_or("").trim_end();
        index = index.saturating_add(1);
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let keyword = words.next().unwrap_or("");
        let rest: Vec<&str> = words.collect();
        let at = format!("{origin}:{index}");
        match keyword {
            "setup" => state.begin_setup(),
            "case" => {
                let id = rest
                    .first()
                    .ok_or_else(|| format!("{at}: `case` needs an id"))?;
                state.start_case(id);
            }
            "arms" => state.set_arms(rest.iter().map(|arm| arm.to_string()).collect()),
            "oracle" => {
                if rest.first() != Some(&"none") {
                    return Err(format!("{at}: the only oracle directive is `oracle none`"));
                }
                state.oracle = false;
            }
            "capability" => {
                let row = rest
                    .first()
                    .ok_or_else(|| format!("{at}: `capability` needs a row name"))?;
                state.add_capability(row);
            }
            "reopen" => state.push(Record::Reopen),
            "statement" => {
                let expect = match rest.first().copied() {
                    Some("ok") => Expect::Ok,
                    Some("error") => Expect::Error(rest.get(1).map(|status| status.to_string())),
                    other => return Err(format!("{at}: `statement {other:?}`")),
                };
                let (sql, next) = take_sql(&lines, index);
                index = next;
                if sql.is_empty() {
                    return Err(format!("{at}: a statement record with no SQL"));
                }
                state.push(Record::Statement { expect, sql });
            }
            "query" => {
                let types = rest.first().copied().unwrap_or("").to_string();
                if types.is_empty()
                    || !types
                        .chars()
                        .all(|letter| matches!(letter, 'T' | 'I' | 'R'))
                {
                    return Err(format!("{at}: a query record with type letters `{types}`"));
                }
                let sort = Sort::parse(rest.get(1).copied().unwrap_or("nosort"))
                    .ok_or_else(|| format!("{at}: unknown sort mode"))?;
                let (sql, next) = take_sql(&lines, index);
                index = next;
                let mut expected = None;
                if lines.get(index).map(|line| line.trim()) == Some("----") {
                    index = index.saturating_add(1);
                    let mut values = Vec::new();
                    while let Some(value) = lines.get(index) {
                        if value.trim().is_empty() {
                            break;
                        }
                        values.push((*value).to_string());
                        index = index.saturating_add(1);
                    }
                    expected = Some(values);
                }
                if sort == Sort::NoSort && !total_order(&sql) {
                    return Err(format!(
                        "{at}: `nosort` on a query whose ORDER BY does not name every result \
                         column; an ordered comparison of an unordered query compares two plans. \
                         Use rowsort."
                    ));
                }
                state.push(Record::Query {
                    types,
                    sort,
                    expected,
                    sql,
                });
            }
            other => return Err(format!("{at}: unknown record `{other}`")),
        }
    }
    state.finish()
}

/// The parser's running state.
struct Parse {
    family: String,
    origin: String,
    oracle: bool,
    file_arms: Option<Vec<String>>,
    file_capabilities: Vec<String>,
    setup: Vec<Record>,
    current: Option<Case>,
    cases: Vec<Case>,
}

impl Parse {
    /// A parser at the top of a file.
    fn new(family: &str, origin: &str) -> Parse {
        Parse {
            family: family.to_string(),
            origin: origin.to_string(),
            oracle: true,
            file_arms: None,
            file_capabilities: Vec::new(),
            setup: Vec::new(),
            current: None,
            cases: Vec::new(),
        }
    }

    /// Closes the case being read, if there is one, and starts another.
    fn start_case(&mut self, id: &str) {
        self.close();
        let mut case = Case::new(&self.family, &self.origin);
        case.id = id.to_string();
        self.current = Some(case);
    }

    /// Starts a new shared setup: the records up to the next `case` replace
    /// the setup every following case runs against.
    fn begin_setup(&mut self) {
        self.close();
        self.setup.clear();
    }

    /// Sets the arms for the current case, or for the file before any case.
    fn set_arms(&mut self, arms: Vec<String>) {
        match self.current.as_mut() {
            Some(case) => case.arms = Some(arms),
            None => self.file_arms = Some(arms),
        }
    }

    /// Adds a capability row to the current case, or to the file.
    fn add_capability(&mut self, row: &str) {
        match self.current.as_mut() {
            Some(case) => case.capabilities.push(row.to_string()),
            None => self.file_capabilities.push(row.to_string()),
        }
    }

    /// Adds a record to the current case, or to the setup.
    fn push(&mut self, record: Record) {
        match self.current.as_mut() {
            Some(case) => case.records.push(record),
            None => self.setup.push(record),
        }
    }

    /// Finishes the case being read.
    fn close(&mut self) {
        if let Some(mut case) = self.current.take() {
            case.setup = self.setup.clone();
            case.oracle = self.oracle;
            if case.arms.is_none() {
                case.arms = self.file_arms.clone();
            }
            let mut capabilities = self.file_capabilities.clone();
            capabilities.append(&mut case.capabilities);
            case.capabilities = capabilities;
            self.cases.push(case);
        }
    }

    /// Returns the file's cases. A file with no `case` line is one case, named
    /// after the family and the file.
    fn finish(mut self) -> Result<CaseFile, String> {
        self.close();
        if self.cases.is_empty() && !self.setup.is_empty() {
            let stem = std::path::Path::new(&self.origin)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut case = Case::new(&self.family, &self.origin);
            case.id = format!("{}-{stem}", self.family);
            case.records = std::mem::take(&mut self.setup);
            case.oracle = self.oracle;
            case.arms = self.file_arms.clone();
            case.capabilities = self.file_capabilities.clone();
            self.cases.push(case);
        }
        Ok(CaseFile { cases: self.cases })
    }
}

/// Reads the SQL that follows a record header, up to a blank line or `----`.
///
/// @param lines - the file
/// @param from - the line after the header
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

/// Whether a query's top level `ORDER BY` has at least one term per result
/// column, which is the rule that lets it be compared in order.
///
/// A result list of `*` cannot be counted from the text, so it never
/// qualifies. The count is of terms, not of distinct columns: `ORDER BY a, a`
/// over two columns passes, and the comparison is then of two plans. The
/// loader's check is a guard against the common mistake, and the grader's
/// ordering check (see `grade.rs`) is what catches the rest.
///
/// @param sql - the query
pub fn total_order(sql: &str) -> bool {
    let Some(order) = top_level_clause(sql, "ORDER BY") else {
        return false;
    };
    let Some(columns) = result_column_count(sql) else {
        return false;
    };
    let terms = split_top_level(order_terms_text(sql, order), b',').len();
    terms >= columns
}

/// Returns the byte offset of a keyword pair at parenthesis depth zero, outside
/// quotes, searching from the end so a compound's final `ORDER BY` is found.
///
/// @param sql - the statement
/// @param phrase - the upper case keywords, separated by one space
pub fn top_level_clause(sql: &str, phrase: &str) -> Option<usize> {
    top_level_spans(sql, phrase).last().map(|(_, end)| *end)
}

/// Returns where every occurrence of a keyword phrase at parenthesis depth
/// zero, outside quotes, starts and ends, in order.
///
/// @param sql - the statement
/// @param phrase - the upper case keywords, separated by one space
pub fn top_level_spans(sql: &str, phrase: &str) -> Vec<(usize, usize)> {
    let upper = sql.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let depths = depth_map(bytes);
    let words: Vec<&str> = phrase.split(' ').collect();
    let Some(first) = words.first() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if depths.get(index).copied() == Some(0) && word_at(bytes, index, first) {
            let mut cursor = index.saturating_add(first.len());
            let mut matched = true;
            for word in words.iter().skip(1) {
                while bytes
                    .get(cursor)
                    .is_some_and(|byte| byte.is_ascii_whitespace())
                {
                    cursor = cursor.saturating_add(1);
                }
                if !word_at(bytes, cursor, word) {
                    matched = false;
                    break;
                }
                cursor = cursor.saturating_add(word.len());
            }
            if matched {
                found.push((index, cursor));
            }
        }
        index = index.saturating_add(1);
    }
    found
}

/// Whether `word` stands alone at `at`.
fn word_at(bytes: &[u8], at: usize, word: &str) -> bool {
    let end = at.saturating_add(word.len());
    let Some(slice) = bytes.get(at..end) else {
        return false;
    };
    if slice != word.as_bytes() {
        return false;
    }
    let before = at
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .is_none_or(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'));
    let after = bytes
        .get(end)
        .is_none_or(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'));
    before && after
}

/// Returns the parenthesis depth at every byte, with quoted text marked as
/// depth one so a keyword inside a string is never read as a clause.
fn depth_map(bytes: &[u8]) -> Vec<u32> {
    let mut depths = Vec::with_capacity(bytes.len());
    let mut depth = 0u32;
    let mut quote: Option<u8> = None;
    for byte in bytes {
        match quote {
            Some(open) => {
                depths.push(1);
                if *byte == open || (open == b'[' && *byte == b']') {
                    quote = None;
                }
                continue;
            }
            None => {}
        }
        match byte {
            b'\'' | b'"' | b'`' | b'[' => {
                quote = Some(*byte);
                depths.push(1);
            }
            b'(' => {
                depths.push(depth);
                depth = depth.saturating_add(1);
            }
            b')' => {
                depth = depth.saturating_sub(1);
                depths.push(depth);
            }
            _ => depths.push(depth),
        }
    }
    depths
}

/// Returns the text of the `ORDER BY` terms, up to `LIMIT` or the end.
fn order_terms_text(sql: &str, from: usize) -> &str {
    let tail = sql.get(from..).unwrap_or("");
    match top_level_clause(tail, "LIMIT") {
        Some(limit) => tail.get(..limit.saturating_sub(5)).unwrap_or(tail),
        None => tail,
    }
}

/// Splits text at a separator byte at depth zero, outside quotes.
///
/// @param text - what to split
/// @param separator - the byte, such as `,`
pub fn split_top_level(text: &str, separator: u8) -> Vec<String> {
    let bytes = text.as_bytes();
    let depths = depth_map(bytes);
    let mut parts = Vec::new();
    let mut start = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == separator && depths.get(index).copied() == Some(0) {
            parts.push(text.get(start..index).unwrap_or("").trim().to_string());
            start = index.saturating_add(1);
        }
    }
    parts.push(text.get(start..).unwrap_or("").trim().to_string());
    parts.retain(|part| !part.is_empty());
    parts
}

/// Counts the result columns of the first `SELECT` at depth zero, or `None`
/// when the list holds a `*` and cannot be counted from the text.
fn result_column_count(sql: &str) -> Option<usize> {
    let upper = sql.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let depths = depth_map(bytes);
    let mut start = None;
    let mut index = 0usize;
    while index < bytes.len() {
        if depths.get(index).copied() == Some(0) && word_at(bytes, index, "SELECT") {
            start = Some(index.saturating_add(6));
            break;
        }
        index = index.saturating_add(1);
    }
    let start = start?;
    let mut end = bytes.len();
    let mut cursor = start;
    while cursor < bytes.len() {
        if depths.get(cursor).copied() == Some(0)
            && [
                "FROM",
                "WHERE",
                "GROUP",
                "ORDER",
                "LIMIT",
                "UNION",
                "EXCEPT",
                "INTERSECT",
                "WINDOW",
            ]
            .iter()
            .any(|word| word_at(bytes, cursor, word))
        {
            end = cursor;
            break;
        }
        cursor = cursor.saturating_add(1);
    }
    let list = sql.get(start..end)?;
    let list = list.trim_start();
    let list = list
        .strip_prefix("DISTINCT ")
        .or_else(|| list.strip_prefix("distinct "))
        .or_else(|| list.strip_prefix("ALL "))
        .unwrap_or(list);
    let terms = split_top_level(list, b',');
    if terms.iter().any(|term| term == "*" || term.ends_with(".*")) {
        return None;
    }
    Some(terms.len())
}

/// Splits a script into statements at the semicolons that end them.
///
/// A semicolon inside a string, a quoted name, a comment, or the body of a
/// `CREATE TRIGGER` does not end a statement. This is the rule SQLite's
/// `sqlite3_complete` follows, written out rather than borrowed from the
/// engine's parser, because a converted script holds statements the engine
/// refuses and the split must not depend on the thing under test.
///
/// @param script - one or more statements
pub fn split_statements(script: &str) -> Vec<String> {
    let bytes = script.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    let mut in_trigger = false;
    let mut block_depth = 0u32;
    let mut words_seen: Vec<String> = Vec::new();
    while index < bytes.len() {
        let byte = bytes.get(index).copied().unwrap_or(0);
        match byte {
            b'\'' | b'"' | b'`' | b'[' => {
                let close = if byte == b'[' { b']' } else { byte };
                index = index.saturating_add(1);
                while index < bytes.len() && bytes.get(index).copied() != Some(close) {
                    index = index.saturating_add(1);
                }
                index = index.saturating_add(1);
                continue;
            }
            b'-' if bytes.get(index.saturating_add(1)).copied() == Some(b'-') => {
                while index < bytes.len() && bytes.get(index).copied() != Some(b'\n') {
                    index = index.saturating_add(1);
                }
                continue;
            }
            b'/' if bytes.get(index.saturating_add(1)).copied() == Some(b'*') => {
                index = index.saturating_add(2);
                while index < bytes.len()
                    && !(bytes.get(index).copied() == Some(b'*')
                        && bytes.get(index.saturating_add(1)).copied() == Some(b'/'))
                {
                    index = index.saturating_add(1);
                }
                index = index.saturating_add(2);
                continue;
            }
            b';' if !in_trigger || block_depth == 0 => {
                push_statement(&mut statements, script.get(start..index).unwrap_or(""));
                start = index.saturating_add(1);
                in_trigger = false;
                block_depth = 0;
                words_seen.clear();
                index = index.saturating_add(1);
                continue;
            }
            _ => {}
        }
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let begin = index;
            while index < bytes.len()
                && bytes
                    .get(index)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                index = index.saturating_add(1);
            }
            let word = script.get(begin..index).unwrap_or("").to_ascii_uppercase();
            if words_seen.len() < 4 {
                words_seen.push(word.clone());
                if words_seen.first().map(String::as_str) == Some("CREATE")
                    && words_seen.iter().any(|seen| seen == "TRIGGER")
                {
                    in_trigger = true;
                }
            }
            if in_trigger {
                match word.as_str() {
                    "BEGIN" | "CASE" => block_depth = block_depth.saturating_add(1),
                    "END" => {
                        block_depth = block_depth.saturating_sub(1);
                        if block_depth == 0 {
                            in_trigger = false;
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }
        index = index.saturating_add(1);
    }
    push_statement(&mut statements, script.get(start..).unwrap_or(""));
    statements
}

/// Adds a statement if it holds anything but whitespace and comments.
fn push_statement(statements: &mut Vec<String>, text: &str) {
    let trimmed = text.trim();
    let meaningful = trimmed
        .lines()
        .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with("--"));
    if meaningful {
        statements.push(trimmed.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file parses into its cases, each carrying the file's setup.
    #[test]
    fn a_file_parses_into_cases_that_share_the_setup() {
        let text = "arms default\ncapability triggers\n\nstatement ok\nCREATE TABLE t(a)\n\ncase one\nquery I rowsort\nSELECT a FROM t\n\ncase two\nstatement error constraint\nINSERT INTO t VALUES(1)\n\nreopen\n";
        let file = parse(text, "select", "x.slt").expect("it parses");
        assert_eq!(file.cases.len(), 2);
        assert_eq!(file.cases[0].id, "one");
        assert_eq!(file.cases[0].setup.len(), 1);
        assert_eq!(file.cases[1].records.len(), 2);
        assert_eq!(file.cases[1].capabilities, vec!["triggers".to_string()]);
        assert_eq!(file.cases[1].arms, Some(vec!["default".to_string()]));
    }

    /// A rendered case parses back to the same records, which is what lets a
    /// shrunk failure be saved and replayed.
    #[test]
    fn a_rendered_case_parses_back() {
        let mut case = Case::new("select", "test");
        case.setup.push(Record::ok("CREATE TABLE t(a)"));
        case.records
            .push(Record::query(Sort::RowSort, "SELECT a FROM t"));
        case.records.push(Record::Reopen);
        case.assign_id();
        let parsed = parse(&case.render(), "select", "test").expect("it parses");
        assert_eq!(parsed.cases.len(), 1);
        assert_eq!(parsed.cases[0].records, case.records);
        assert_eq!(parsed.cases[0].setup, case.setup);
        assert_eq!(parsed.cases[0].id, case.id);
    }

    /// An id does not change when the SQL is re-indented.
    #[test]
    fn an_id_ignores_whitespace() {
        let mut one = Case::new("select", "a");
        one.records
            .push(Record::query(Sort::RowSort, "SELECT  a\n FROM t"));
        one.assign_id();
        let mut two = Case::new("select", "b");
        two.records
            .push(Record::query(Sort::RowSort, "SELECT a FROM t"));
        two.assign_id();
        assert_eq!(one.id, two.id);
        assert!(one.id.starts_with("select-"));
        assert_eq!(one.id.len(), "select-".len() + 12);
    }

    /// `nosort` is refused unless the ORDER BY has a term per column.
    #[test]
    fn nosort_needs_a_total_order() {
        assert!(total_order("SELECT a, b FROM t ORDER BY a, b"));
        assert!(total_order("SELECT a, b FROM t ORDER BY 1, 2 LIMIT 3"));
        assert!(!total_order("SELECT a, b FROM t ORDER BY a"));
        assert!(!total_order("SELECT * FROM t ORDER BY a"));
        assert!(!total_order("SELECT a FROM t"));
        assert!(!total_order("SELECT a FROM (SELECT a FROM t ORDER BY a)"));
        assert!(parse("query I nosort\nSELECT a, b FROM t ORDER BY a\n", "f", "x").is_err());
    }

    /// Semicolons in strings, comments and trigger bodies do not split.
    #[test]
    fn a_script_splits_at_the_semicolons_that_end_statements() {
        let script = "CREATE TABLE t(a); INSERT INTO t VALUES('a;b'); -- c;\nCREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a = CASE WHEN 1 THEN 2 END; DELETE FROM t; END; SELECT 1";
        let statements = split_statements(script);
        assert_eq!(statements.len(), 4, "{statements:#?}");
        assert!(statements[2].starts_with("CREATE TRIGGER"));
        assert!(statements[2].ends_with("END"));
    }

    /// The read only test counts a bare pragma read and refuses a pragma write.
    #[test]
    fn reads_are_told_from_writes() {
        assert!(is_read_only("SELECT 1"));
        assert!(is_read_only("WITH c AS (SELECT 1) SELECT * FROM c"));
        assert!(is_read_only("PRAGMA user_version"));
        assert!(!is_read_only("PRAGMA user_version = 3"));
        assert!(!is_read_only("INSERT INTO t VALUES(1)"));
        assert!(!is_read_only("WITH c AS (SELECT 1) DELETE FROM t"));
    }
}
