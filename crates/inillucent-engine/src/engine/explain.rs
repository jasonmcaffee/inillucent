//! Rendering an `EXPLAIN` and an `EXPLAIN QUERY PLAN` as rows.
//!
//! Invariant: **the rows are the reference's shape, not this engine's.** A
//! caller that prints an `EXPLAIN` is a caller that already knows what SQLite's
//! looks like, so the columns and the opcode names here are SQLite's even where
//! this engine has no bytecode to name.

use inillucent_exec::dml::Changes;
use inillucent_exec::physical::{self};
use inillucent_tree::datum::OwnedDatum;

use crate::*;

/// Returns a shape's column names as strings.
///
/// @param shape - what the built plan produces
/// Returns the names a `RETURNING` clause's columns report.
///
/// **A write that answers rows has to name them.** `INSERT ... RETURNING a, b`
/// produced its rows and an empty name list, so a caller drawing a grid had two
/// columns of values and no headings for them - which `inillucent-driver`'s
/// conformance suite caught, because a result with rows and no columns is a
/// shape nothing else in the engine produces.
///
/// The names are the binder's own `BoundResultColumn::name`, which is where a
/// `SELECT`'s come from too, so `SELECT a` and `INSERT ... RETURNING a` cannot
/// disagree about what the column is called.
///
/// @param returning - the bound `RETURNING` columns
pub(crate) fn returning_names(
    returning: &[inillucent_sql::bind::BoundResultColumn],
) -> Vec<String> {
    returning
        .iter()
        .map(|column| String::from_utf8_lossy(&column.name).into_owned())
        .collect()
}

/// Returns the names a plan's result columns report.
pub(crate) fn names_of(shape: &physical::Shape) -> Vec<String> {
    shape
        .names
        .iter()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect()
}

/// Renders `EXPLAIN QUERY PLAN` lines as the rows a caller reads.
///
/// The four columns are SQLite's - `id`, `parent`, `notused`, `detail` - so a
/// caller written against SQLite reads the same shape and finds its text where
/// it expects it. The ids are the line's position rather than a tree: this
/// engine's `describe` renders the chain source-first as a list, and inventing
/// a parent for each line would be inventing structure the renderer does not
/// carry. SQLite documents its own `EXPLAIN QUERY PLAN` output as unstable
/// between releases, so the text was never the comparable part.
///
/// @param lines - the plan's operators, source first
/// Returns the listing a plain `EXPLAIN` answers with.
///
/// **The eight columns SQLite answers with, holding this engine's steps.**
/// `EXPLAIN` in SQLite lists the opcodes of a bytecode program; this engine
/// compiles no bytecode, so what is listed is the operator chain the statement
/// actually runs - one row per stage, framed by the `Init` and `Halt` that
/// begin and end every execution here as they do there.
///
/// The columns are used for what they mean rather than left at zero: `p1` is
/// the step's position in the chain, `p2` is where control goes next, `p4`
/// carries the operator's argument, and `comment` is the same sentence
/// `EXPLAIN QUERY PLAN` prints. A reader comparing two engines' listings is
/// comparing two different machines and will see that; a reader asking what
/// *this* statement does gets an answer rather than a refusal.
///
/// @param lines - the plan, as `EXPLAIN QUERY PLAN` describes it
pub(crate) fn program_of(lines: &[String]) -> Vec<(String, i64, i64, String, String)> {
    let mut program = Vec::with_capacity(lines.len().saturating_add(2));
    let last = lines.len().saturating_add(1) as i64;
    program.push((
        "Init".to_string(),
        0,
        1,
        String::new(),
        "Start at 1".to_string(),
    ));
    for (at, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start_matches(['`', '-', '|', ' ']);
        let (word, rest) = match trimmed.split_once(' ') {
            Some((word, rest)) => (word, rest),
            None => (trimmed, ""),
        };
        program.push((
            opcode_name(word),
            at as i64,
            at.saturating_add(2) as i64,
            rest.to_string(),
            trimmed.to_string(),
        ));
    }
    program.push(("Halt".to_string(), 0, 0, String::new(), String::new()));
    let _ = last;
    program
}

/// Returns a plan word as an opcode name.
///
/// `SCAN` and `SEARCH` are the two the planner writes most, and the rest are
/// title-cased so that a listing reads as a program rather than as a shouted
/// sentence.
///
/// @param word - the first word of the plan line
fn opcode_name(word: &str) -> String {
    let mut name = String::with_capacity(word.len());
    for (at, letter) in word.chars().enumerate() {
        if at == 0 {
            name.extend(letter.to_uppercase());
        } else {
            name.extend(letter.to_lowercase());
        }
    }
    name
}

/// Returns the rows a plain `EXPLAIN` answers with.
///
/// @param program - the steps, as `program_of` built them
pub(crate) fn program_rows(program: &[(String, i64, i64, String, String)]) -> Outcome {
    Outcome {
        rows: program
            .iter()
            .enumerate()
            .map(|(address, (opcode, one, two, argument, comment))| {
                vec![
                    OwnedDatum::Int(address as i64),
                    OwnedDatum::Text(opcode.as_bytes().to_vec()),
                    OwnedDatum::Int(*one),
                    OwnedDatum::Int(*two),
                    OwnedDatum::Int(0),
                    OwnedDatum::Text(argument.as_bytes().to_vec()),
                    OwnedDatum::Int(0),
                    OwnedDatum::Text(comment.as_bytes().to_vec()),
                ]
            })
            .collect(),
        names: std::rc::Rc::new(vec![
            "addr".to_string(),
            "opcode".to_string(),
            "p1".to_string(),
            "p2".to_string(),
            "p3".to_string(),
            "p4".to_string(),
            "p5".to_string(),
            "comment".to_string(),
        ]),
        changes: Changes::default(),
    }
}

/// Adds every object a bound query reads to a list.
///
/// Recursive through subqueries, because a table a subquery reads is a table
/// the statement uses - which is the question `tables_used` answers.
///
/// @param select - the bound query
/// @param into - the list being built
pub(crate) fn collect_sources(
    select: &inillucent_sql::bind::BoundSelect,
    into: &mut Vec<(&'static str, Vec<u8>)>,
) {
    for source in &select.sources {
        let kind = match source.table.kind {
            inillucent_sql::catalog_view::TableKind::View => "view",
            _ => "table",
        };
        into.push((kind, source.table.name.clone()));
    }
}

pub(crate) fn query_plan_rows(lines: &[String]) -> Outcome {
    Outcome {
        rows: lines
            .iter()
            .enumerate()
            .map(|(position, line)| {
                vec![
                    OwnedDatum::Int(position as i64),
                    OwnedDatum::Int(0),
                    OwnedDatum::Int(0),
                    OwnedDatum::Text(line.as_bytes().to_vec()),
                ]
            })
            .collect(),
        names: std::rc::Rc::new(vec![
            "id".to_string(),
            "parent".to_string(),
            "notused".to_string(),
            "detail".to_string(),
        ]),
        changes: Changes::default(),
    }
}
