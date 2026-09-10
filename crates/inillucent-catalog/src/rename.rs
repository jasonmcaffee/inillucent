//! Rewriting the stored `CREATE` text when a name changes.
//!
//! Invariant: a rewrite replaces *tokens*, never bytes matched by search. The
//! text a schema row holds is the text a person wrote, comments and all, and
//! `ALTER TABLE` has to give it back changed in exactly one respect. A textual
//! substitution would rewrite the inside of a string literal, the middle of a
//! longer identifier and the word in a comment, and every one of those produces
//! a schema that still parses and means something else.
//!
//! The second invariant is that a rewrite is checked before it is kept. The
//! result is re-parsed, and an `ALTER` whose rewrite does not parse is refused
//! whole rather than written - which is what stops a rename leaving a database
//! whose schema cannot be loaded.

use inillucent_base::limits::Limits;
use inillucent_base::{error, DbResult};
use inillucent_sql::ast::Statement;
use inillucent_sql::lexer::{Lexer, Span, TokenKind};
use inillucent_sql::parser::parse_next_statement;

/// Which kind of name a rewrite is changing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rename {
    /// A table's name, which appears wherever a table is named.
    Table,
    /// A column's name, which appears wherever a column is named.
    Column,
}

/// Returns the stored SQL with one name replaced by another.
///
/// The occurrences replaced are the ones in a *naming* position, decided by the
/// token before them. That is a smaller rule than SQLite's full re-resolution
/// and it is deliberately conservative: a token this does not recognise as a
/// name position is left alone, so the failure mode is an unrewritten reference
/// that the caller's re-parse then rejects, rather than a silent corruption.
pub fn rewrite(sql: &[u8], kind: Rename, from: &[u8], to: &[u8]) -> DbResult<Vec<u8>> {
    let folded = from.to_ascii_lowercase();
    // **A table is written quoted and a column is written as typed.** SQLite's
    // `ALTER TABLE` substitutes `"%w"` for a new *table* name unconditionally,
    // so `RENAME TO people` leaves `CREATE TABLE "people" (...)`; a new *column*
    // name goes in as the author wrote it, so `RENAME COLUMN email TO address`
    // leaves `address` bare. The two are compared against SQLite byte for byte,
    // so the asymmetry is copied rather than tidied away.
    let replacement = match kind {
        Rename::Table => quoted(to),
        Rename::Column => quote_if_needed(to),
    };
    let mut edits: Vec<Span> = Vec::new();
    let mut lexer = Lexer::at(sql, 0);
    let mut previous: Option<Vec<u8>> = None;
    loop {
        let token = lexer
            .next_token()
            .map_err(|reason| error::corrupt(format!("schema SQL does not lex: {reason:?}")))?;
        if token.kind == TokenKind::EndOfInput {
            break;
        }
        let TokenKind::Identifier { keyword, .. } = token.kind else {
            previous = Some(token.span.slice(sql).to_ascii_lowercase());
            continue;
        };
        let text = unquoted(token.span.slice(sql));
        let names_it = if keyword.is_some() {
            // A keyword is never the name being renamed: `ALTER TABLE t RENAME
            // TO order` writes `"order"`, and an unquoted `order` in the text is
            // the keyword rather than the table.
            false
        } else {
            text.to_ascii_lowercase() == folded
        };
        if names_it && in_naming_position(kind, previous.as_deref()) {
            edits.push(token.span);
        }
        previous = Some(token.span.slice(sql).to_ascii_lowercase());
    }
    Ok(splice(sql, &edits, &replacement))
}

/// Returns whether a token following the given one names the kind being
/// renamed.
///
/// For a table: after `table`, `on`, `from`, `join`, `into`, `update`, or a
/// comma - the last because a comma-joined FROM list has no keyword of its own.
/// For a column: after an opening parenthesis, a comma, an operator or a
/// keyword that introduces an expression. The column case is broader because a
/// column can appear almost anywhere, and the narrowing is done instead by the
/// caller: it only rewrites objects that the column's own table owns.
fn in_naming_position(kind: Rename, previous: Option<&[u8]>) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    match kind {
        Rename::Table => matches!(
            previous,
            b"table" | b"on" | b"from" | b"join" | b"into" | b"update" | b"," | b"exists"
        ),
        Rename::Column => !matches!(
            previous,
            b"table" | b"index" | b"view" | b"trigger" | b"as" | b"." | b"exists"
        ),
    }
}

/// Applies the edits to the source, replacing each span with the replacement.
fn splice(source: &[u8], edits: &[Span], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(source.len());
    let mut cursor = 0usize;
    for span in edits {
        let start = span.start as usize;
        let end = span.end as usize;
        if start < cursor || end > source.len() {
            continue;
        }
        out.extend_from_slice(source.get(cursor..start).unwrap_or(&[]));
        out.extend_from_slice(replacement);
        cursor = end;
    }
    out.extend_from_slice(source.get(cursor..).unwrap_or(&[]));
    out
}

/// Returns an identifier's text with its quoting removed.
fn unquoted(raw: &[u8]) -> Vec<u8> {
    let Some(first) = raw.first().copied() else {
        return Vec::new();
    };
    let closing = match first {
        b'"' => b'"',
        b'`' => b'`',
        b'[' => b']',
        _ => return raw.to_vec(),
    };
    let inner = raw
        .get(1..raw.len().saturating_sub(1))
        .unwrap_or(&[])
        .to_vec();
    if closing == b']' {
        return inner;
    }
    // A doubled quote inside the identifier is one character.
    let mut out = Vec::with_capacity(inner.len());
    let mut index = 0usize;
    while index < inner.len() {
        let byte = inner.get(index).copied().unwrap_or(0);
        out.push(byte);
        index = index.saturating_add(1);
        if byte == closing && inner.get(index).copied() == Some(closing) {
            index = index.saturating_add(1);
        }
    }
    out
}

/// Returns a slice with its leading and trailing ASCII whitespace removed.
///
/// @param bytes - the slice
fn trimmed(bytes: &[u8]) -> &[u8] {
    let mut start = 0usize;
    let mut end = bytes.len();
    while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
        start = start.saturating_add(1);
    }
    while end > start
        && bytes
            .get(end.saturating_sub(1))
            .is_some_and(u8::is_ascii_whitespace)
    {
        end = end.saturating_sub(1);
    }
    bytes.get(start..end).unwrap_or(&[])
}

/// Returns a name quoted only when it needs to be to survive a re-parse.
///
/// A name that is a keyword, holds a space, or does not start with a letter has
/// to be written quoted - otherwise the rewritten schema parses as something
/// else, or does not parse at all. Anything else goes in bare, which is what
/// SQLite writes for a renamed column.
///
/// @param name - the name to write
pub fn quote_if_needed(name: &[u8]) -> Vec<u8> {
    let plain = name
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        && inillucent_sql::keyword::lookup(&name.to_ascii_lowercase()).is_none();
    if plain {
        return name.to_vec();
    }
    quoted(name)
}

/// Returns a name written as a quoted identifier.
///
/// **Always quoted, even when the name would parse bare.** SQLite's own
/// `ALTER TABLE` substitutes `"%w"` for the new name without asking whether it
/// needs the quotes, so `ALTER TABLE t RENAME TO people` leaves
/// `CREATE TABLE "people" (...)` in `sqlite_schema`. Quoting only when the name
/// demands it produces text that means the same thing and is not the same
/// bytes - and the stored `CREATE` text is compared byte for byte against
/// SQLite's, because it is what a reader re-parses to learn what the table is.
///
/// The quoting rules the old version applied are still what makes the *escape*
/// correct: a keyword, a space, a leading digit or an embedded quote all have
/// to survive the re-parse, and doubling an interior `"` is what does it.
///
/// @param name - the name to write
pub fn quoted(name: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len().saturating_add(2));
    out.push(b'"');
    for byte in name {
        if *byte == b'"' {
            out.push(b'"');
        }
        out.push(*byte);
    }
    out.push(b'"');
    out
}

/// Checks that a rewritten statement still parses, returning it unchanged.
///
/// The check is the whole reason a rewrite is safe to keep. An `ALTER` that
/// produced text the parser cannot read would leave a database whose schema
/// fails to load, and that is not recoverable from inside the engine.
///
/// These failures stay `Corrupt`, unlike the stored-schema failures, which
/// were moved off `Corrupt` in `load.rs`. The distinction is whose text it
/// is: a stored `CREATE TABLE` is text SQLite wrote and the caller can read,
/// so a parse failure there is a gap in this engine's grammar and says so.
/// Text *this* engine just generated and cannot read back is an internal
/// defect nothing the caller wrote can cause, and filing it under the
/// caller's typos would hide it.
pub fn reparsed(sql: Vec<u8>) -> DbResult<Vec<u8>> {
    let limits = Limits::default();
    let parsed = parse_next_statement(&sql, 0, &limits).map_err(|reason| {
        error::corrupt(format!(
            "the rewritten schema does not parse: {}",
            reason.message()
        ))
    })?;
    let sane = matches!(
        parsed.statement,
        Statement::CreateTable { .. }
            | Statement::CreateIndex { .. }
            | Statement::CreateView { .. }
            | Statement::CreateTrigger { .. }
    );
    if !sane {
        return Err(error::corrupt("the rewritten schema is not a CREATE"));
    }
    Ok(sql)
}

/// Returns the `CREATE TABLE` text with one column definition appended.
///
/// The column goes before the closing parenthesis of the column list, which is
/// the last one that closes the list rather than the last one in the text: a
/// table with a `CHECK (a > 0)` has parentheses after it.
pub fn add_column(sql: &[u8], definition: &[u8]) -> DbResult<Vec<u8>> {
    let Some(at) = column_list_end(sql) else {
        return Err(error::corrupt("a CREATE TABLE with no column list"));
    };
    // **Trimmed, because the separator is written here.** The definition is a
    // slice of the statement's own source and the span the binder recorded
    // starts at the whitespace after `ADD COLUMN`, so appending it after a
    // literal `", "` left `..., INTEGER,  joined TEXT` - two spaces where
    // SQLite writes one, and the stored text is compared byte for byte.
    let definition = trimmed(definition);
    let mut out = Vec::with_capacity(sql.len().saturating_add(definition.len()).saturating_add(2));
    out.extend_from_slice(sql.get(..at).unwrap_or(&[]));
    out.extend_from_slice(b", ");
    out.extend_from_slice(definition);
    out.extend_from_slice(sql.get(at..).unwrap_or(&[]));
    Ok(out)
}

/// Returns the offset of the parenthesis that closes the column list.
fn column_list_end(sql: &[u8]) -> Option<usize> {
    let mut lexer = Lexer::at(sql, 0);
    let mut depth = 0usize;
    let mut opened = false;
    loop {
        let token = lexer.next_token().ok()?;
        match token.kind {
            TokenKind::EndOfInput => return None,
            TokenKind::Punctuator(inillucent_sql::lexer::Punctuator::LeftParen) => {
                depth = depth.saturating_add(1);
                opened = true;
            }
            TokenKind::Punctuator(inillucent_sql::lexer::Punctuator::RightParen) => {
                depth = depth.saturating_sub(1);
                if opened && depth == 0 {
                    return Some(token.span.start as usize);
                }
            }
            _ => {}
        }
    }
}

/// Returns the tables a stored `CREATE` text names, folded.
///
/// A view or trigger that reads one table can have a column renamed inside it
/// unambiguously; one that reads two cannot, because a bare column name in it
/// might belong to either. Knowing which case a row is in is the difference
/// between rewriting it correctly and rewriting the wrong table's column.
pub fn referenced_tables(sql: &[u8]) -> Vec<Vec<u8>> {
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut lexer = Lexer::at(sql, 0);
    let mut previous: Option<Vec<u8>> = None;
    loop {
        let Ok(token) = lexer.next_token() else {
            return names;
        };
        if token.kind == TokenKind::EndOfInput {
            return names;
        }
        let raw = token.span.slice(sql);
        if let TokenKind::Identifier { keyword: None, .. } = token.kind {
            let names_a_table = matches!(
                previous.as_deref(),
                Some(b"on") | Some(b"from") | Some(b"join") | Some(b"into") | Some(b"update")
            );
            if names_a_table {
                let folded = unquoted(raw).to_ascii_lowercase();
                if !names.contains(&folded) {
                    names.push(folded);
                }
            }
        }
        previous = Some(raw.to_ascii_lowercase());
    }
}

/// Returns the `CREATE TABLE` text with one column definition removed.
///
/// The definition's own span comes from re-parsing, so the cut is exactly the
/// column and its separating comma - not a byte range guessed from the name,
/// which would take the wrong half of `a INTEGER, ab TEXT`.
pub fn drop_column(sql: &[u8], position: usize) -> DbResult<Vec<u8>> {
    let limits = Limits::default();
    // The text being read here is the *stored* `CREATE TABLE`, so a failure is
    // the same fact the import path reports and is reported the same way: the
    // statement could not be parsed, which is not a claim about the disk. The
    // rewrite self-checks below stay corruption on purpose - see `reparsed`.
    let parsed = parse_next_statement(sql, 0, &limits)
        .map_err(|reason| crate::load::unparseable_schema("CREATE TABLE", reason))?;
    let Statement::CreateTable {
        body: inillucent_sql::ast::CreateTableBody::Columns { columns, .. },
        ..
    } = &parsed.statement
    else {
        return Err(crate::load::corrupt_schema(
            "the schema SQL for this table is not a CREATE TABLE",
        ));
    };
    let Some(doomed) = columns.get(position) else {
        return Err(crate::load::corrupt_schema(
            "the column to drop is not in the schema",
        ));
    };
    let start = doomed.span.start as usize;
    let end = doomed.span.end as usize;
    // Take the comma with it: the one before when this is the last column, the
    // one after otherwise, so the list never ends up with a doubled or dangling
    // separator.
    let (cut_start, cut_end) = if position.saturating_add(1) < columns.len() {
        (start, next_comma(sql, end).unwrap_or(end).saturating_add(1))
    } else {
        (previous_comma(sql, start).unwrap_or(start), end)
    };
    let mut out = Vec::with_capacity(sql.len());
    out.extend_from_slice(sql.get(..cut_start).unwrap_or(&[]));
    out.extend_from_slice(sql.get(cut_end..).unwrap_or(&[]));
    Ok(out)
}

/// Returns the offset of the first comma at or after a position.
fn next_comma(sql: &[u8], from: usize) -> Option<usize> {
    sql.get(from..)?
        .iter()
        .position(|byte| *byte == b',')
        .map(|at| from.saturating_add(at))
}

/// Returns the offset of the last comma before a position.
fn previous_comma(sql: &[u8], before: usize) -> Option<usize> {
    sql.get(..before)?.iter().rposition(|byte| *byte == b',')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renaming replaces the table's own name and nothing that merely looks
    /// like it - not the text inside a string, not a longer identifier, and not
    /// a column that happens to share the name.
    #[test]
    fn a_rename_replaces_tokens_rather_than_bytes() {
        let sql = b"CREATE TABLE t (t TEXT DEFAULT 't', ts INTEGER, note TEXT DEFAULT 'about t')";
        let out = rewrite(sql, Rename::Table, b"t", b"u").expect("it rewrites");
        assert_eq!(
            String::from_utf8_lossy(&out),
            // Quoted, because SQLite quotes a renamed table unconditionally.
            "CREATE TABLE \"u\" (t TEXT DEFAULT 't', ts INTEGER, note TEXT DEFAULT 'about t')"
        );
    }

    /// An index names its table after `ON`, and that is the occurrence a table
    /// rename has to change.
    #[test]
    fn an_index_follows_its_table() {
        let sql = b"CREATE INDEX t_name ON t (name)";
        let out = rewrite(sql, Rename::Table, b"t", b"u").expect("it rewrites");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "CREATE INDEX t_name ON \"u\" (name)"
        );
    }

    /// A new name that is a keyword comes back quoted, because an unquoted one
    /// would parse as the keyword.
    #[test]
    fn a_keyword_name_is_quoted() {
        assert_eq!(quoted(b"order"), b"\"order\"".to_vec());
        assert_eq!(quoted(b"two words"), b"\"two words\"".to_vec());
        // Quoted even when it would have parsed bare, which is what SQLite
        // writes and what the stored text is compared against.
        assert_eq!(quoted(b"plain"), b"\"plain\"".to_vec());
        assert_eq!(quoted(b"a\"b"), b"\"a\"\"b\"".to_vec());
    }

    /// A column is added inside the list rather than after whatever closes the
    /// statement.
    #[test]
    fn a_column_is_added_inside_the_list() {
        let sql = b"CREATE TABLE t (a INTEGER, b TEXT CHECK (b <> ''))";
        let out = add_column(sql, b"c REAL").expect("it appends");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "CREATE TABLE t (a INTEGER, b TEXT CHECK (b <> ''), c REAL)"
        );
    }

    /// A column is cut with its separator, and the name is not searched for -
    /// so `a` goes and `ab` stays.
    #[test]
    fn a_dropped_column_takes_its_comma() {
        let sql = b"CREATE TABLE t (a INTEGER, ab TEXT, c REAL)";
        let out = drop_column(sql, 0).expect("it drops");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "CREATE TABLE t ( ab TEXT, c REAL)"
        );
        let last = drop_column(sql, 2).expect("it drops");
        assert_eq!(
            String::from_utf8_lossy(&last),
            "CREATE TABLE t (a INTEGER, ab TEXT)"
        );
    }

    /// A view that reads one table can have its columns renamed; one that
    /// reads two is ambiguous, and the caller has to know which it is.
    #[test]
    fn the_tables_a_statement_reads_are_listed() {
        assert_eq!(
            referenced_tables(b"CREATE VIEW v AS SELECT a FROM t WHERE a > 1"),
            vec![b"t".to_vec()]
        );
        assert_eq!(
            referenced_tables(b"CREATE VIEW v AS SELECT a FROM t JOIN u ON t.k = u.k"),
            vec![b"t".to_vec(), b"u".to_vec()]
        );
        assert_eq!(
            referenced_tables(b"CREATE INDEX i ON t (name)"),
            vec![b"t".to_vec()]
        );
    }

    /// A rewrite that does not parse is refused rather than kept.
    #[test]
    fn an_unparseable_rewrite_is_refused() {
        assert!(reparsed(b"CREATE TABLE".to_vec()).is_err());
        assert!(reparsed(b"SELECT 1".to_vec()).is_err());
        assert!(reparsed(b"CREATE TABLE t (a)".to_vec()).is_ok());
    }
}
