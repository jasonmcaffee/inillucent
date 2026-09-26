//! Deciding whether two answers to one statement agree.
//!
//! Invariant: **errors are compared by result code and never by wording, rows
//! are compared in order only when the query asked for an order, and a real is
//! compared by its bits except for the transcendental functions named in
//! [`ONE_ULP_FUNCTIONS`].** The one message that is compared is a `RAISE`,
//! whose text is the value under test; `differential::observation_differences`
//! handles it, and this module reuses that function rather than restating it.
//!
//! An `unsupported` answer where SQLite succeeded is a **gap**, not a wrong
//! answer, and it passes only when the case names a capability row that says
//! `no` or `partial`. That keeps `inillucent capabilities` honest over every
//! construct in the matrix instead of over one probe per row.

use std::path::{Path, PathBuf};

use crate::differential::observation_differences_counting;
use crate::oracle::{Observation, TaggedValue};
use crate::statement_matrix::case::{Case, Expect, Record, Sort};
use crate::statement_matrix::properties::row_order;

/// What kind of difference failed a case. A shrink keeps a case only while it
/// fails with the same kind on the same statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// One engine succeeded and the other failed.
    Outcome,
    /// Both failed, with different result codes or `RAISE` messages.
    Status,
    /// Different rows, or rows in a different order under `nosort`.
    Rows,
    /// Different column names.
    Columns,
    /// Different `changes`, `total_changes`, `last_insert_rowid` or
    /// `autocommit`.
    Counters,
    /// The engine refused with `unsupported` where SQLite answered, and no
    /// capability row the case names says `no` or `partial`.
    Gap,
    /// SQLite itself did not do what the case file says, so the file is wrong.
    Expectation,
    /// A difference that appeared only after the reopen.
    Reopen,
    /// `PRAGMA integrity_check` did not answer `ok`.
    Integrity,
    /// The engine panicked.
    Panic,
    /// A property of section 6.3 did not hold.
    Property,
    /// The setup the case shares with others failed.
    Fixture,
    /// The oracle process could not answer twice in a row.
    Oracle,
    /// The recorded answer of an `oracle none` case did not match.
    Recorded,
}

/// Every kind, for the lists that name kinds by word.
pub const KINDS: &[Kind] = &[
    Kind::Outcome,
    Kind::Status,
    Kind::Rows,
    Kind::Columns,
    Kind::Counters,
    Kind::Gap,
    Kind::Expectation,
    Kind::Reopen,
    Kind::Integrity,
    Kind::Panic,
    Kind::Property,
    Kind::Fixture,
    Kind::Oracle,
    Kind::Recorded,
];

/// One failed case.
#[derive(Clone, Debug)]
pub struct Failure {
    /// The case id.
    pub case: String,
    /// What kind of difference it was.
    pub kind: Kind,
    /// Which record, counting from zero, or `None` for the case as a whole.
    pub statement: Option<usize>,
    /// The statement's SQL, when there is one.
    pub sql: String,
    /// What differed.
    pub detail: String,
    /// The scratch directory, which is kept for a failing case.
    pub directory: PathBuf,
}

impl Failure {
    /// Renders the failure for a test's panic message.
    pub fn render(&self) -> String {
        format!(
            "{} [{:?}] record {}: {}\n      {}\n      files: {}",
            self.case,
            self.kind,
            self.statement
                .map(|index| index.to_string())
                .unwrap_or_else(|| "-".to_string()),
            self.sql.replace('\n', " "),
            self.detail.replace('\n', "\n      "),
            self.directory.display()
        )
    }
}

/// How a case ended.
#[derive(Clone, Debug)]
pub enum Verdict {
    /// Every comparison agreed.
    Passed,
    /// At least one did not. The first failure is the one a shrink preserves.
    Failed(Vec<Failure>),
    /// The case did not run, and why.
    Skipped(String),
}

/// The transcendental math functions whose results may differ from SQLite's
/// in the last place, because SQLite calls the C library and inillucent calls
/// Rust's. The tolerance is one unit in the last place and applies to a query
/// only when it calls one of these; nowhere else is a real compared by
/// anything but its bits.
pub const ONE_ULP_FUNCTIONS: &[&str] = &[
    "exp", "ln", "log", "log10", "log2", "pow", "power", "sin", "cos", "tan", "asin", "acos",
    "atan", "atan2", "sinh", "cosh", "tanh", "asinh", "acosh", "atanh",
];

/// What grading one case needs to know, besides the records.
pub struct CaseContext<'a> {
    /// The case.
    pub case: &'a Case,
    /// Its scratch directory.
    pub directory: &'a Path,
    /// Whether the oracle grades it.
    pub graded: bool,
    /// Whether the cumulative counters are comparable.
    pub counters: bool,
    /// How many setup records run inside the case before its own.
    pub setup_count: usize,
    /// Whether grading has stopped: set by an allowed gap on a statement that
    /// writes, after which the two databases no longer hold the same rows and
    /// nothing later in the case can be compared.
    pub halted: bool,
    /// Whether a `CREATE VIRTUAL TABLE` has run since the last write, which
    /// leaves SQLite's `changes()` holding what the module did to its shadow
    /// tables (see `observation_differences_counting`).
    pub module_pending: bool,
    /// Where failures are collected.
    pub failures: &'a mut Vec<Failure>,
}

impl CaseContext<'_> {
    /// Records one failure.
    ///
    /// @param kind - what kind of difference
    /// @param statement - which record, if one
    /// @param sql - the record's SQL
    /// @param detail - what differed
    pub fn fail(&mut self, kind: Kind, statement: Option<usize>, sql: &str, detail: String) {
        self.failures.push(Failure {
            case: self.case.id.clone(),
            kind,
            statement,
            sql: sql.to_string(),
            detail,
            directory: self.directory.to_path_buf(),
        });
    }

    /// Names a record for a message: setup statements and case statements are
    /// counted separately, as the file shows them.
    ///
    /// @param index - the position in the script
    pub fn label(&self, index: usize) -> String {
        if index < self.setup_count {
            format!("setup {index}")
        } else {
            format!("record {}", index.saturating_sub(self.setup_count))
        }
    }

    /// Whether an `unsupported` answer is allowed, because a capability row
    /// the case names says the engine does not do it.
    pub fn gap_allowed(&self) -> bool {
        self.case.capabilities.iter().any(|row| {
            matches!(
                inillucent_driver::capability::supports(row),
                Some(inillucent_driver::capability::Support::No)
                    | Some(inillucent_driver::capability::Support::Partial)
            )
        })
    }
}

/// What both engines said about one statement.
pub struct Asked {
    /// inillucent's observation.
    pub candidate: Observation,
    /// The feature inillucent said it has not built, if that was its answer.
    pub unsupported: Option<String>,
    /// inillucent's status name, when it failed.
    pub status: Option<String>,
    /// SQLite's observation, when the case is graded.
    pub reference: Option<Observation>,
}

/// Grades one record against both answers, or against its own expectation
/// when there is no oracle.
///
/// @param record - the record
/// @param asked - what the engines said
/// @param index - its position in the script
/// @param context - the case
/// @param reopened - whether this is the rerun after the final reopen
pub fn grade(
    record: &Record,
    asked: &Asked,
    index: usize,
    context: &mut CaseContext<'_>,
    reopened: bool,
) {
    let label = if reopened {
        format!("after reopen, {}", context.label(index))
    } else {
        context.label(index)
    };
    match &asked.reference {
        None => grade_alone(record, asked, index, context, &label),
        Some(reference) => grade_pair(record, reference, asked, index, context, &label, reopened),
    }
    if let Some(sql) = record.sql() {
        let head: String = sql
            .trim_start()
            .chars()
            .take(21)
            .collect::<String>()
            .to_ascii_uppercase();
        if head.starts_with("CREATE VIRTUAL TABLE") {
            context.module_pending = true;
        } else if ["INSERT", "UPDATE", "DELETE", "REPLACE"]
            .iter()
            .any(|verb| head.starts_with(verb))
        {
            context.module_pending = false;
        }
    }
}

/// Checks a statement record's expectation against what SQLite did, and the
/// status the case names against what inillucent said. Returns whether
/// grading should go on.
fn expectation_holds(
    record: &Record,
    reference: &Observation,
    asked: &Asked,
    index: usize,
    context: &mut CaseContext<'_>,
) -> bool {
    let Record::Statement { expect, sql } = record else {
        return true;
    };
    let expected_ok = matches!(expect, Expect::Ok);
    if expected_ok != reference.ok {
        context.fail(
            Kind::Expectation,
            Some(index),
            sql,
            format!(
                "the case says `statement {}` and SQLite {}: {}",
                if expected_ok { "ok" } else { "error" },
                if reference.ok { "succeeded" } else { "failed" },
                reference.message
            ),
        );
        return false;
    }
    if let (Expect::Error(Some(wanted)), false, Some(said)) =
        (expect, asked.candidate.ok, asked.status.as_deref())
    {
        if wanted != said {
            context.fail(
                Kind::Status,
                Some(index),
                sql,
                format!("the case expects status `{wanted}` and inillucent said `{said}`"),
            );
            return false;
        }
    }
    true
}

/// Grades one record against the oracle's answer.
fn grade_pair(
    record: &Record,
    reference: &Observation,
    asked: &Asked,
    index: usize,
    context: &mut CaseContext<'_>,
    label: &str,
    reopened: bool,
) {
    let sql = record.sql().unwrap_or("");
    let kind_of = |kind: Kind| if reopened { Kind::Reopen } else { kind };
    let candidate = &asked.candidate;
    if !expectation_holds(record, reference, asked, index, context) {
        return;
    }
    // A gap the case names is accepted whether SQLite answered or refused for
    // a reason of its own: the engine did not get as far as asking itself the
    // question SQLite's refusal answers.
    if !candidate.ok && asked.unsupported.is_some() && context.gap_allowed() {
        if !crate::statement_matrix::case::is_read_only(sql) {
            context.halted = true;
        }
        return;
    }
    if reference.ok && !candidate.ok && asked.unsupported.is_some() {
        context.fail(
            kind_of(Kind::Gap),
            Some(index),
            sql,
            format!(
                "{label}: inillucent has not built `{}` and SQLite answered; the case names \
                 no capability row that says `no` or `partial` ({})",
                asked.unsupported.as_deref().unwrap_or(""),
                candidate.message
            ),
        );
        return;
    }
    let differences = observation_differences_counting(
        label,
        sql,
        candidate,
        reference,
        false,
        context.counters,
        !context.module_pending,
    );
    if let Some(first) = differences.first() {
        let kind = if candidate.ok != reference.ok {
            Kind::Outcome
        } else if !reference.ok {
            Kind::Status
        } else {
            Kind::Counters
        };
        context.fail(kind_of(kind), Some(index), sql, first.clone());
        return;
    }
    let Record::Query { sort, .. } = record else {
        return;
    };
    if !reference.ok {
        return;
    }
    if let Some(detail) = explain_difference(sql, reference, candidate) {
        context.fail(
            kind_of(Kind::Rows),
            Some(index),
            sql,
            format!("{label}: {detail}"),
        );
        return;
    }
    if is_explain(sql) {
        return;
    }
    let columns = named_as_bound(record, &candidate.columns);
    if crate::statement_matrix::case::asks_for_rows(sql) && columns != reference.columns {
        let detail = format!(
            "{label}: column names\n  inillucent: {:?}\n  SQLite:  {:?}",
            columns, reference.columns
        );
        context.fail(kind_of(Kind::Columns), Some(index), sql, detail);
        return;
    }
    if let Some(detail) = compare_rows(*sort, &reference.rows, &candidate.rows, tolerant(sql)) {
        context.fail(
            kind_of(Kind::Rows),
            Some(index),
            sql,
            format!("{label}: {detail}"),
        );
    }
}

/// inillucent's column names with the first run's bound values written in,
/// the way the oracle was asked.
///
/// The name of a result column that is an expression is the expression's
/// text. The oracle was sent the statement with its parameters replaced by
/// literals (see `bind.rs`), so its names hold `2` where this engine's hold
/// `?1`; the comparison is of what each engine was asked.
fn named_as_bound(record: &Record, columns: &[String]) -> Vec<String> {
    match record {
        Record::Query { binds, .. } if !binds.is_empty() => {
            // The oracle's names are from its last run, which is the one
            // whose observation the comparison keeps.
            let first = binds.last().cloned().unwrap_or_default();
            columns
                .iter()
                .map(|name| crate::statement_matrix::bind::substitute(name, &first))
                .collect()
        }
        _ => columns.to_vec(),
    }
}

/// Grades a record with no oracle: against its expectation and its recorded
/// answer, when it has them.
fn grade_alone(
    record: &Record,
    asked: &Asked,
    index: usize,
    context: &mut CaseContext<'_>,
    label: &str,
) {
    let candidate = &asked.candidate;
    match record {
        Record::Statement { expect, sql } => {
            let ok = matches!(expect, Expect::Ok);
            if ok != candidate.ok {
                let detail = format!(
                    "{label}: the case says `statement {}` and inillucent {}: {}",
                    if ok { "ok" } else { "error" },
                    if candidate.ok { "succeeded" } else { "failed" },
                    candidate.message
                );
                context.fail(Kind::Recorded, Some(index), sql, detail);
                return;
            }
            if let (Expect::Error(Some(wanted)), Some(said)) = (expect, asked.status.as_deref()) {
                if wanted != said {
                    let detail =
                        format!("the case expects status `{wanted}` and inillucent said `{said}`");
                    context.fail(Kind::Status, Some(index), sql, detail);
                }
            }
        }
        Record::Query {
            sort,
            expected: Some(expected),
            sql,
            ..
        } => {
            if !candidate.ok {
                let detail = format!("{label}: refused: {}", candidate.message);
                context.fail(Kind::Recorded, Some(index), sql, detail);
                return;
            }
            let rendered = render_rows(&candidate.rows, *sort);
            if &rendered != expected {
                let detail =
                    format!("{label}: answered {rendered:?}, and the case records {expected:?}");
                context.fail(Kind::Recorded, Some(index), sql, detail);
            }
        }
        Record::Query { .. } | Record::Reopen => {}
    }
}

/// Whether a statement is an `EXPLAIN` of either kind.
fn is_explain(sql: &str) -> bool {
    sql.trim_start()
        .get(..7)
        .is_some_and(|head| head.eq_ignore_ascii_case("EXPLAIN"))
}

/// Grades an `EXPLAIN QUERY PLAN` by the tables it names, and an `EXPLAIN` by
/// nothing but having run.
///
/// Section 4.1 of the design: a plan's text and its node numbers are the
/// planner's own and differ between two correct engines, so what is compared
/// is that each engine's plan reads the same tables, as a sorted list of the
/// names that follow `SCAN` and `SEARCH`. `SCAN CONSTANT ROW` names no table.
/// A bytecode `EXPLAIN` lists one engine's instructions, which the other engine
/// does not have, so only its outcome is compared, which the caller has done.
///
/// @param sql - the statement
/// @param reference - SQLite's answer
/// @param candidate - inillucent's answer
fn explain_difference(
    sql: &str,
    reference: &Observation,
    candidate: &Observation,
) -> Option<String> {
    let upper = sql.trim_start().to_ascii_uppercase();
    if !upper.starts_with("EXPLAIN QUERY PLAN") {
        return None;
    }
    let theirs = plan_tables(&reference.rows);
    let ours = plan_tables(&candidate.rows);
    if theirs == ours {
        return None;
    }
    Some(format!(
        "the plans read different tables
  inillucent: {ours:?}
  SQLite:  {theirs:?}"
    ))
}

/// The table names a query plan scans or searches, sorted.
///
/// @param rows - the plan's rows; the detail is the last column
pub fn plan_tables(rows: &[Vec<TaggedValue>]) -> Vec<String> {
    let mut names = Vec::new();
    for row in rows {
        let Some(TaggedValue::Text(detail)) = row.last() else {
            continue;
        };
        let text = String::from_utf8_lossy(detail);
        let words: Vec<&str> = text.split_whitespace().collect();
        for pair in words.windows(2) {
            if let [verb, name] = pair {
                if (*verb == "SCAN" || *verb == "SEARCH") && *name != "CONSTANT" {
                    names.push(
                        name.trim_matches(|c: char| c == ',' || c == '(')
                            .to_string(),
                    );
                }
            }
        }
    }
    names.sort();
    names
}

/// Compares two row lists under a sort mode, returning what differed.
///
/// @param sort - how the rows are compared
/// @param reference - SQLite's rows
/// @param candidate - inillucent's rows
/// @param tolerant - whether a real may differ by one unit in the last place
pub fn compare_rows(
    sort: Sort,
    reference: &[Vec<TaggedValue>],
    candidate: &[Vec<TaggedValue>],
    tolerant: bool,
) -> Option<String> {
    let theirs = arrange(reference, sort);
    let ours = arrange(candidate, sort);
    if theirs.len() != ours.len() {
        return Some(format!(
            "{} row(s) from inillucent and {} from SQLite\n  inillucent: {:?}\n  SQLite:  {:?}",
            ours.len(),
            theirs.len(),
            candidate.iter().take(8).collect::<Vec<_>>(),
            reference.iter().take(8).collect::<Vec<_>>()
        ));
    }
    for (position, (left, right)) in ours.iter().zip(theirs.iter()).enumerate() {
        let same = left.len() == right.len()
            && left
                .iter()
                .zip(right.iter())
                .all(|(a, b)| a.identical(b) || (tolerant && within_one_ulp(a, b)));
        if !same {
            return Some(format!(
                "rows differ at {} {position}\n  inillucent: {left:?}\n  SQLite:  {right:?}",
                sort.word()
            ));
        }
    }
    None
}

/// Puts rows into the order a sort mode compares them in.
fn arrange(rows: &[Vec<TaggedValue>], sort: Sort) -> Vec<Vec<TaggedValue>> {
    match sort {
        Sort::NoSort => rows.to_vec(),
        Sort::RowSort => {
            let mut sorted = rows.to_vec();
            sorted.sort_by(|left, right| row_order(left, right));
            sorted
        }
        Sort::ValueSort => {
            let mut values: Vec<Vec<TaggedValue>> = rows
                .iter()
                .flatten()
                .map(|value| vec![value.clone()])
                .collect();
            values.sort_by(|left, right| row_order(left, right));
            values
        }
    }
}

/// Whether two reals are at most one unit in the last place apart.
fn within_one_ulp(left: &TaggedValue, right: &TaggedValue) -> bool {
    match (left, right) {
        (TaggedValue::Real(a), TaggedValue::Real(b)) => {
            let (a, b) = (a.to_bits() as i64, b.to_bits() as i64);
            (a >= 0) == (b >= 0) && a.abs_diff(b) <= 1
        }
        _ => false,
    }
}

/// Whether a query calls a function in [`ONE_ULP_FUNCTIONS`].
///
/// @param sql - the query
pub fn tolerant(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    ONE_ULP_FUNCTIONS.iter().any(|name| {
        let needle = format!("{name}(");
        lower.match_indices(&needle).any(|(at, _)| {
            at == 0
                || lower
                    .as_bytes()
                    .get(at.saturating_sub(1))
                    .is_some_and(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'))
        })
    })
}

/// Renders rows the way sqllogictest writes a result block.
///
/// @param rows - the values
/// @param sort - the sort mode the block was recorded under
pub fn render_rows(rows: &[Vec<TaggedValue>], sort: Sort) -> Vec<String> {
    let mut rendered: Vec<Vec<String>> = rows
        .iter()
        .map(|row| row.iter().map(render_value).collect())
        .collect();
    match sort {
        Sort::NoSort => rendered.into_iter().flatten().collect(),
        Sort::RowSort => {
            rendered.sort();
            rendered.into_iter().flatten().collect()
        }
        Sort::ValueSort => {
            let mut values: Vec<String> = rendered.into_iter().flatten().collect();
            values.sort();
            values
        }
    }
}

/// Renders one value as sqllogictest does.
fn render_value(value: &TaggedValue) -> String {
    let text = match value {
        TaggedValue::Null => return "NULL".to_string(),
        TaggedValue::Integer(integer) => integer.to_string(),
        TaggedValue::Real(real) => crate::slt::format_real(*real),
        TaggedValue::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        TaggedValue::Blob(bytes) => crate::hash::to_hex(bytes),
    };
    if text.is_empty() {
        "(empty)".to_string()
    } else {
        text
    }
}

/// Whether an integrity check answered exactly `ok`.
///
/// @param observation - what `PRAGMA integrity_check` returned
pub fn integrity_ok(observation: &Observation) -> bool {
    observation.ok
        && observation.rows.len() == 1
        && observation
            .rows
            .first()
            .and_then(|row| row.first())
            .is_some_and(|value| *value == TaggedValue::Text(b"ok".to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Row order matters only under `nosort`.
    #[test]
    fn rows_are_compared_under_their_sort_mode() {
        let one = vec![vec![TaggedValue::Integer(1)], vec![TaggedValue::Integer(2)]];
        let two = vec![vec![TaggedValue::Integer(2)], vec![TaggedValue::Integer(1)]];
        assert!(compare_rows(Sort::RowSort, &one, &two, false).is_none());
        assert!(compare_rows(Sort::NoSort, &one, &two, false).is_some());
        assert!(compare_rows(Sort::NoSort, &one, &one, false).is_none());
    }

    /// The one unit tolerance applies only to the named functions, and only to
    /// one unit.
    #[test]
    fn the_tolerance_is_one_unit_and_only_for_the_named_functions() {
        let a = 0.1f64;
        let b = f64::from_bits(a.to_bits() + 1);
        let c = f64::from_bits(a.to_bits() + 2);
        let left = vec![vec![TaggedValue::Real(a)]];
        let near = vec![vec![TaggedValue::Real(b)]];
        let far = vec![vec![TaggedValue::Real(c)]];
        assert!(compare_rows(Sort::NoSort, &left, &near, true).is_none());
        assert!(compare_rows(Sort::NoSort, &left, &near, false).is_some());
        assert!(compare_rows(Sort::NoSort, &left, &far, true).is_some());
        assert!(tolerant("SELECT exp(a) FROM t"));
        assert!(tolerant("SELECT round(sin(1), 3)"));
        assert!(!tolerant("SELECT expected(a) FROM t"));
        assert!(!tolerant("SELECT sqrt(2)"));
    }
}
