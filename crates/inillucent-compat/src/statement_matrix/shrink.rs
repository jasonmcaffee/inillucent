//! Reducing a failing case to the smallest one that still fails the same way
//! (section 6.5 of the design).
//!
//! Invariant: **a reduction is kept only when the reduced case still fails
//! with the same kind of difference on the same statement text.** Shrinking
//! that accepted any failure would wander from the defect it started with to
//! a different one, and the case it saved would describe the wrong bug.
//!
//! The steps, each repeated until none of its removals keeps the failure:
//!
//! 1. remove one record or setup statement;
//! 2. remove one row from a multi row `VALUES` list;
//! 3. remove one property;
//! 4. remove a clause: a `WHERE` term joined by `AND` or `OR`, an `ORDER BY`,
//!    a `LIMIT`.
//!
//! Each attempt runs the whole case on both engines, so shrinking costs tens
//! of runs; it runs only for a case that failed.

use crate::statement_matrix::case::{split_top_level, top_level_spans, Case, Record};
use crate::statement_matrix::grade::{Failure, Kind, Verdict};
use crate::statement_matrix::run::Runner;

/// What identifies a failure for the purpose of shrinking.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Signature {
    kind: Kind,
    sql: String,
}

impl Signature {
    /// The signature of a failure.
    fn of(failure: &Failure) -> Signature {
        // A property failure has no statement; its name, which the detail
        // starts with, says which property it was.
        let text = if failure.sql.is_empty() {
            failure.detail.split(':').next().unwrap_or("")
        } else {
            &failure.sql
        };
        Signature {
            kind: failure.kind,
            sql: text.split_whitespace().collect::<Vec<&str>>().join(" "),
        }
    }
}

/// Whether a verdict holds a failure with the signature.
fn still_fails(verdict: &Verdict, wanted: &Signature) -> bool {
    match verdict {
        Verdict::Failed(failures) => failures
            .iter()
            .any(|failure| Signature::of(failure) == *wanted),
        _ => false,
    }
}

/// Shrinks a case that fails on a runner, returning the smallest case found
/// and how many runs it took. Returns `None` when the case does not fail.
///
/// @param runner - a runner at the arm the case failed at
/// @param case - the failing case
pub fn shrink(runner: &mut Runner, case: &Case) -> Option<(Case, usize)> {
    let first = match runner.run(case) {
        Verdict::Failed(failures) => failures.first().cloned()?,
        _ => return None,
    };
    let wanted = Signature::of(&first);
    let mut best = case.clone();
    let mut runs = 1usize;
    let mut attempt = |candidate: &Case, runs: &mut usize| -> bool {
        *runs = runs.saturating_add(1);
        let mut probe = candidate.clone();
        probe.id = format!("{}-shrink{}", case.id, runs);
        still_fails(&runner.run(&probe), &wanted)
    };
    loop {
        let before = size(&best);
        for step in [
            remove_records,
            remove_rows,
            remove_properties,
            remove_clauses,
        ] {
            let mut changed = true;
            while changed {
                changed = false;
                for candidate in step(&best) {
                    if attempt(&candidate, &mut runs) {
                        best = candidate;
                        changed = true;
                        break;
                    }
                }
            }
        }
        if size(&best) >= before {
            break;
        }
    }
    best.id = case.id.clone();
    Some((best, runs))
}

/// A measure of a case's size, which every step reduces.
fn size(case: &Case) -> usize {
    case.setup
        .iter()
        .chain(case.records.iter())
        .map(|record| record.sql().map(str::len).unwrap_or(1))
        .sum::<usize>()
        .saturating_add(case.properties.len().saturating_mul(1000))
}

/// Every case with one record or one setup statement removed.
fn remove_records(case: &Case) -> Vec<Case> {
    let mut out = Vec::new();
    for at in 0..case.setup.len() {
        let mut smaller = case.clone();
        smaller.setup.remove(at);
        out.push(smaller);
    }
    for at in 0..case.records.len() {
        let mut smaller = case.clone();
        smaller.records.remove(at);
        out.push(smaller);
    }
    out
}

/// Every case with one row removed from one multi row `VALUES` list.
fn remove_rows(case: &Case) -> Vec<Case> {
    let mut out = Vec::new();
    let lists = |records: &[Record]| -> Vec<(usize, Vec<String>)> {
        records
            .iter()
            .enumerate()
            .filter_map(|(at, record)| Some((at, values_rows(record.sql()?)?)))
            .collect()
    };
    for (in_setup, found) in [(true, lists(&case.setup)), (false, lists(&case.records))] {
        for (at, rows) in found {
            if rows.len() < 2 {
                continue;
            }
            for drop in 0..rows.len() {
                let kept: Vec<String> = rows
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != drop)
                    .map(|(_, row)| row.clone())
                    .collect();
                let mut smaller = case.clone();
                let target = if in_setup {
                    &mut smaller.setup
                } else {
                    &mut smaller.records
                };
                if let Some(record) = target.get_mut(at) {
                    replace_values(record, &kept);
                }
                out.push(smaller);
            }
        }
    }
    out
}

/// The rows of a statement's top level `VALUES` list, when it has one.
fn values_rows(sql: &str) -> Option<Vec<String>> {
    let (_, end) = top_level_spans(sql, "VALUES").first().copied()?;
    let tail = sql.get(end..)?;
    let stop = ["ON CONFLICT", "RETURNING"]
        .iter()
        .filter_map(|phrase| {
            top_level_spans(tail, phrase)
                .first()
                .map(|(start, _)| *start)
        })
        .min()
        .unwrap_or(tail.len());
    let rows = split_top_level(tail.get(..stop)?, b',');
    rows.iter().all(|row| row.starts_with('(')).then_some(rows)
}

/// Rewrites a record's `VALUES` list to the given rows.
fn replace_values(record: &mut Record, rows: &[String]) {
    let sql = match record {
        Record::Statement { sql, .. } | Record::Query { sql, .. } => sql,
        Record::Reopen => return,
    };
    let Some((_, end)) = top_level_spans(sql, "VALUES").first().copied() else {
        return;
    };
    let tail = sql.get(end..).unwrap_or("").to_string();
    let stop = ["ON CONFLICT", "RETURNING"]
        .iter()
        .filter_map(|phrase| {
            top_level_spans(&tail, phrase)
                .first()
                .map(|(start, _)| *start)
        })
        .min()
        .unwrap_or(tail.len());
    let head = sql.get(..end).unwrap_or("").to_string();
    let rest = tail.get(stop..).unwrap_or("").to_string();
    *sql = format!("{head} {} {rest}", rows.join(", "))
        .trim_end()
        .to_string();
}

/// Every case with one property removed.
fn remove_properties(case: &Case) -> Vec<Case> {
    (0..case.properties.len())
        .map(|at| {
            let mut smaller = case.clone();
            smaller.properties.remove(at);
            smaller
        })
        .collect()
}

/// Every case with one clause removed from one statement: a `WHERE` term
/// joined by `AND` or `OR`, the whole `WHERE`, an `ORDER BY` or a `LIMIT`.
fn remove_clauses(case: &Case) -> Vec<Case> {
    let mut out = Vec::new();
    for (in_setup, records) in [(true, &case.setup), (false, &case.records)] {
        for (at, record) in records.iter().enumerate() {
            let Some(sql) = record.sql() else {
                continue;
            };
            for smaller_sql in clause_removals(sql) {
                let mut smaller = case.clone();
                let target = if in_setup {
                    &mut smaller.setup
                } else {
                    &mut smaller.records
                };
                if let Some(slot) = target.get_mut(at) {
                    set_sql(slot, smaller_sql);
                }
                out.push(smaller);
            }
        }
    }
    out
}

/// The statements one clause shorter than a statement.
fn clause_removals(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    for phrase in ["LIMIT", "ORDER BY"] {
        if let Some((start, _)) = top_level_spans(sql, phrase).last().copied() {
            if let Some(head) = sql.get(..start) {
                out.push(head.trim_end().to_string());
            }
        }
    }
    for phrase in [" AND ", " OR "] {
        let word = phrase.trim();
        for (start, end) in top_level_spans(sql, word) {
            // Drop the term after the keyword up to the next top level keyword.
            let before = sql.get(..start).unwrap_or("");
            let after = sql.get(end..).unwrap_or("");
            let next = [
                "AND",
                "OR",
                "ORDER BY",
                "LIMIT",
                "GROUP BY",
                "UNION",
                "EXCEPT",
                "INTERSECT",
            ]
            .iter()
            .filter_map(|keyword| top_level_spans(after, keyword).first().map(|(at, _)| *at))
            .min()
            .unwrap_or(after.len());
            let rest = after.get(next..).unwrap_or("");
            out.push(
                format!("{} {}", before.trim_end(), rest.trim_start())
                    .trim()
                    .to_string(),
            );
        }
    }
    out
}

/// Replaces a record's SQL.
fn set_sql(record: &mut Record, new: String) {
    match record {
        Record::Statement { sql, .. } | Record::Query { sql, .. } => *sql = new,
        Record::Reopen => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A VALUES list is found and rewritten without its other clauses.
    #[test]
    fn values_rows_are_found_and_replaced() {
        let sql = "INSERT INTO t VALUES (1, 'a'), (2, 'b,c'), (3, NULL) ON CONFLICT DO NOTHING";
        let rows = values_rows(sql).expect("it has rows");
        assert_eq!(rows.len(), 3);
        let mut record = Record::ok(sql);
        replace_values(&mut record, &rows[..1].to_vec());
        assert_eq!(
            record.sql(),
            Some("INSERT INTO t VALUES (1, 'a') ON CONFLICT DO NOTHING")
        );
    }

    /// A clause removal drops one term and leaves the rest.
    #[test]
    fn a_term_is_removed() {
        let removals = clause_removals("SELECT a FROM t WHERE a > 1 AND b < 2 ORDER BY a LIMIT 3");
        assert!(removals.contains(&"SELECT a FROM t WHERE a > 1 AND b < 2 ORDER BY a".to_string()));
        assert!(removals.contains(&"SELECT a FROM t WHERE a > 1 ORDER BY a LIMIT 3".to_string()));
    }
}
