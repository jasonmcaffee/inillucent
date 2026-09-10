//! Reading a virtual table's declaration and a pragma's argument.
//!
//! Invariant: nothing here touches a database. These are four pure functions
//! over types this crate already owns, a `Declaration` and a `PragmaArgument`,
//! and they are here rather than in a connection crate because **both** engines
//! need them and neither should have to depend on the other to get them.
//!
//! They lived in `inillucent-session`, which is the old engine's connection, and
//! the new engine's statement path imported them from there. That was the last
//! thing tying the new engine to the old one that was not itself an engine: two
//! helpers that parse text. Moving them down removes the edge without changing
//! a caller, because `inillucent-session` re-exports both under their old paths.

use crate::bind::BoundExpr;
use crate::catalog_view::ColumnInfo;
use crate::directive::PragmaArgument;
use crate::vtab::Declaration;

/// Returns the columns a module's declaration provides.
///
/// A declared column carries a name, a declared type, an affinity, a collation
/// and whether it is hidden. Everything else a `ColumnInfo` can say - NOT NULL,
/// a default, a primary-key position, a generated expression - is something a
/// `CREATE TABLE` says and a module's declaration does not, so it is left at
/// the value that means "unsaid" rather than guessed at.
///
/// @param declaration - what the module answered when it was connected
pub fn declared_columns(declaration: &Declaration) -> Vec<ColumnInfo> {
    declaration
        .columns
        .iter()
        .map(|column| ColumnInfo {
            folded: column.name.to_ascii_lowercase(),
            name: column.name.clone(),
            declared_type: column.declared_type.clone(),
            affinity: column.affinity,
            collation: column.collation.clone(),
            not_null: false,
            not_null_conflict: None,
            primary_key_conflict: None,
            default_sql: None,
            primary_key_position: None,
            hidden: column.hidden,
            generated: false,
            stored: true,
            generated_sql: None,
        })
        .collect()
}

/// Reads a pragma argument as text.
///
/// @param argument - the argument as the parser produced it
pub fn argument_text(argument: &PragmaArgument) -> String {
    match argument {
        PragmaArgument::Name(name) => String::from_utf8_lossy(name).into_owned(),
        PragmaArgument::Value(expr) => expression_text(expr),
    }
}

/// Returns the text a bound pragma argument spells.
///
/// `PRAGMA cache_size = -4000` is a unary minus over a literal rather than a
/// negative literal, because that is what the grammar has. Reading only the
/// literal made every negative setting read as zero.
///
/// @param expr - the argument's expression
fn expression_text(expr: &BoundExpr) -> String {
    match expr {
        BoundExpr::Text(text) => String::from_utf8_lossy(text).into_owned(),
        BoundExpr::Integer(value) => value.to_string(),
        BoundExpr::Real(value) => value.to_string(),
        BoundExpr::Unary { op, operand } => match op {
            crate::ast::UnaryOp::Negate => format!("-{}", expression_text(operand)),
            crate::ast::UnaryOp::Identity => expression_text(operand),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// Reads a pragma argument as the boolean SQLite accepts.
///
/// SQLite reads `on`, `yes` and `true` as one and everything else it cannot
/// parse as zero, which is why `PRAGMA foreign_keys = maybe` turns them off.
///
/// @param argument - the argument as the parser produced it
pub fn argument_boolean(argument: &PragmaArgument) -> bool {
    let text = argument_text(argument);
    let folded = text.trim().to_ascii_lowercase();
    match folded.as_str() {
        "on" | "yes" | "true" => true,
        "off" | "no" | "false" => false,
        _ => folded
            .parse::<i64>()
            .map(|value| value != 0)
            .unwrap_or(false),
    }
}

/// Reads a pragma argument as an integer.
///
/// @param argument - the argument as the parser produced it
pub fn argument_integer(argument: &PragmaArgument) -> i64 {
    argument_text(argument).trim().parse().unwrap_or(0)
}
