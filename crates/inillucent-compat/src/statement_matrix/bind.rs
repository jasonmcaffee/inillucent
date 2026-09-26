//! Bound parameters: the binding axis of section 4.2 of the design.
//!
//! Invariant: **a statement run with bound values is compared with the same
//! statement run on SQLite with those values written into its text as
//! literals, which SQLite defines to mean the same thing.** A bound value and
//! a literal both have no affinity, so the two forms must answer alike; what
//! the axis tests is inillucent's binding path, its parameter numbering and
//! its reuse of a prepared statement, against an oracle that answers the
//! question without them. The oracle protocol's own `bind` returns one row
//! and refuses a statement that returns none, so it cannot carry these cases,
//! and the oracle binary is shared by every checkout and is not rebuilt here.
//!
//! A record may carry several runs. inillucent then prepares the statement
//! once and runs it once per set of values, resetting it between runs; the
//! rows of every run are compared together, in run order, and the counters
//! after the last.

use inillucent_engine::connect::Connection;
use inillucent_tree::datum::OwnedDatum;

use crate::differential::tagged;
use crate::oracle::Observation;
use crate::statement_matrix::case::split_top_level;

/// Parses one SQL literal: an integer, a real, a quoted string, a blob or
/// `NULL`.
///
/// @param text - the literal as it is written in SQL
pub fn literal_datum(text: &str) -> Result<OwnedDatum, String> {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("NULL") {
        return Ok(OwnedDatum::Null);
    }
    if let Some(body) = trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return Ok(OwnedDatum::Text(body.replace("''", "'").into_bytes()));
    }
    if let Some(body) = trimmed
        .strip_prefix("x'")
        .or_else(|| trimmed.strip_prefix("X'"))
        .and_then(|rest| rest.strip_suffix('\''))
    {
        let mut bytes = Vec::with_capacity(body.len() / 2);
        let mut index = 0usize;
        while index < body.len() {
            let pair = body
                .get(index..index.saturating_add(2))
                .ok_or_else(|| format!("`{trimmed}` has an odd number of hex digits"))?;
            bytes.push(u8::from_str_radix(pair, 16).map_err(|_| format!("`{pair}` is not hex"))?);
            index = index.saturating_add(2);
        }
        return Ok(OwnedDatum::Blob(bytes));
    }
    if let Ok(integer) = trimmed.parse::<i64>() {
        return Ok(OwnedDatum::Int(integer));
    }
    if let Ok(real) = trimmed.parse::<f64>() {
        return Ok(OwnedDatum::Real(real));
    }
    Err(format!("`{trimmed}` is not a literal a bind line can hold"))
}

/// Splits a `bind` line's values, which are separated by `;` outside quotes.
///
/// @param line - the text after `bind `
pub fn split_values(line: &str) -> Vec<String> {
    split_top_level(line, b';')
}

/// Writes literals into a statement in place of its parameters, numbering the
/// parameters as SQLite does: `?N` is N, a bare `?` is one more than the
/// largest so far, and a named parameter takes the next number the first time
/// its name appears.
///
/// @param sql - the statement
/// @param literals - the values for parameters 1, 2, and so on
pub fn substitute(sql: &str, literals: &[String]) -> String {
    let characters: Vec<(usize, char)> = sql.char_indices().collect();
    let mut out = String::with_capacity(sql.len());
    let mut names: Vec<(String, usize)> = Vec::new();
    let mut largest = 0usize;
    let mut index = 0usize;
    let at = |position: usize| characters.get(position).map(|(_, character)| *character);
    let offset = |position: usize| {
        characters
            .get(position)
            .map(|(byte, _)| *byte)
            .unwrap_or(sql.len())
    };
    while let Some(character) = at(index) {
        if character == '\'' || character == '"' {
            let mut close = index.saturating_add(1);
            while at(close).is_some_and(|next| next != character) {
                close = close.saturating_add(1);
            }
            out.push_str(
                sql.get(offset(index)..offset(close.saturating_add(1)))
                    .unwrap_or(""),
            );
            index = close.saturating_add(1);
            continue;
        }
        let named = matches!(character, ':' | '@' | '$')
            && at(index.saturating_add(1))
                .is_some_and(|next| next.is_ascii_alphabetic() || next == '_');
        if character != '?' && !named {
            out.push(character);
            index = index.saturating_add(1);
            continue;
        }
        let start = index;
        index = index.saturating_add(1);
        while at(index).is_some_and(|next| next.is_ascii_alphanumeric() || next == '_') {
            index = index.saturating_add(1);
        }
        let token = sql.get(offset(start)..offset(index)).unwrap_or("");
        let number = if character == '?' {
            match token
                .get(1..)
                .and_then(|digits| digits.parse::<usize>().ok())
            {
                Some(number) => number,
                None => largest.saturating_add(1),
            }
        } else {
            match names.iter().find(|(name, _)| name == token) {
                Some((_, number)) => *number,
                None => {
                    let number = largest.saturating_add(1);
                    names.push((token.to_string(), number));
                    number
                }
            }
        };
        largest = largest.max(number);
        out.push_str(
            literals
                .get(number.saturating_sub(1))
                .map(String::as_str)
                .unwrap_or("NULL"),
        );
    }
    out
}

/// Runs one statement on inillucent once per set of values, on one prepared
/// statement, and reports it the way the oracle would.
///
/// @param connection - the session
/// @param sql - one statement with parameters
/// @param runs - the values for each run, as SQL literals
pub fn observe_bound(
    connection: &Connection<'_>,
    sql: &str,
    runs: &[Vec<String>],
) -> (Observation, Option<inillucent_base::DbError>) {
    let mut observation = Observation::default();
    let outcome = (|| -> Result<(), (inillucent_base::DbError, bool)> {
        let mut statement = connection.prepare(sql).map_err(|error| (error, true))?;
        for (number, run) in runs.iter().enumerate() {
            if number > 0 {
                statement.reset();
            }
            for (at, literal) in run.iter().enumerate() {
                let value = literal_datum(literal)
                    .map_err(|reason| (inillucent_base::error::misuse(reason), true))?;
                let index = u32::try_from(at.saturating_add(1)).unwrap_or(u32::MAX);
                statement
                    .bind(index, value)
                    .map_err(|error| (error, true))?;
            }
            while statement.step().map_err(|error| (error, true))? {
                if observation.columns.is_empty() {
                    observation.columns = statement.columns().to_vec();
                }
                observation
                    .rows
                    .push(statement.row().iter().map(tagged).collect());
            }
            if observation.columns.is_empty() {
                observation.columns = statement.columns().to_vec();
            }
        }
        Ok(())
    })();
    let mut error = None;
    if let Err((failure, _)) = outcome {
        observation.ok = false;
        observation.code = failure.code().value();
        observation.extended = failure.extended().value();
        observation.message = failure.message().to_string();
        observation.rows.clear();
        observation.columns.clear();
        error = Some(failure);
    }
    observation.changes = connection.changes().unwrap_or_default();
    observation.total_changes = connection.total_changes().unwrap_or_default();
    observation.last_insert_rowid = connection.last_insert_rowid().unwrap_or_default();
    observation.autocommit = connection.autocommit().unwrap_or_default();
    (observation, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parameters are numbered the way SQLite numbers them.
    #[test]
    fn literals_replace_parameters_by_sqlite_numbering() {
        let values = vec!["1".to_string(), "'x'".to_string(), "NULL".to_string()];
        assert_eq!(substitute("SELECT ?1, ?2", &values), "SELECT 1, 'x'");
        assert_eq!(substitute("SELECT ?, ?", &values), "SELECT 1, 'x'");
        assert_eq!(substitute("SELECT :a, :b, :a", &values), "SELECT 1, 'x', 1");
        assert_eq!(substitute("SELECT ?2, ?", &values), "SELECT 'x', NULL");
        assert_eq!(substitute("SELECT '?1', ?1", &values), "SELECT '?1', 1");
        assert_eq!(substitute("SELECT 'é', ?1", &values), "SELECT 'é', 1");
    }

    /// Each literal form becomes the value it writes.
    #[test]
    fn literals_parse_into_values() {
        assert_eq!(literal_datum("NULL").ok(), Some(OwnedDatum::Null));
        assert_eq!(literal_datum("-7").ok(), Some(OwnedDatum::Int(-7)));
        assert_eq!(literal_datum("2.5").ok(), Some(OwnedDatum::Real(2.5)));
        assert_eq!(
            literal_datum("'it''s'").ok(),
            Some(OwnedDatum::Text(b"it's".to_vec()))
        );
        assert_eq!(
            literal_datum("x'00ff'").ok(),
            Some(OwnedDatum::Blob(vec![0, 255]))
        );
        assert!(literal_datum("abc").is_err());
    }
}
