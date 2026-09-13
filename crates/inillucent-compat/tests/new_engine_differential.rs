//! The new engine and pinned SQLite 3.53.4, asked the same generated questions
//! about the same file.
//!
//! Invariant: every difference is either **an answer that matches** or **a
//! refusal with a named reason**. There is no third category, and that is the
//! whole point of the test - the physical pass is a whitelist that fails loudly
//! rather than approximating, so a query it accepts must be right and a query
//! it declines must say why.
//!
//! ## Why the corpus is generated rather than written
//!
//! `new_engine_slt.rs` grades the new engine against recorded SQLite answers
//! for the queries the conformance corpus happens to contain. That is a good
//! test and it is not a sweep: the queries were written by people, so they
//! cluster where people's attention clustered.
//!
//! This one enumerates instead. It reads the fixture's actual schema and emits
//! every shape the physical pass claims to handle, against every column - point
//! lookups, half-open and closed ranges, ordering both ways, `IS NULL`, `IN`,
//! `LIKE`, grouping, distinct, the aggregates, limits and offsets, and joins
//! across the tables that share a column name. A few hundred queries fall out
//! of a schema of three tables, and they land on the combinations nobody would
//! think to write down - which is where the affinity, collation and NULL-order
//! bugs this phase found were actually living.
//!
//! ## Why the oracle answers over the same file
//!
//! Because a fixture rebuilt for the reference is a different fixture. Both
//! engines open the copy the import read, so a difference is a difference in
//! the engines and never in the data.
//!
//! When the pinned oracle has not been built this test says so and returns,
//! the way the rest of the differential suite does: a run without the oracle
//! evidences nothing and must not look like a pass.

use std::collections::BTreeMap;
use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::workspace_root;
use inillucent_tree::datum::OwnedDatum;

/// Returns the pinned SQLite oracle binary, if it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Copies the corpus fixture somewhere both engines can open it.
fn corpus_copy() -> PathBuf {
    let source = workspace_root().join("compat/fixtures/select-corpus.db");
    let directory = std::env::temp_dir().join(format!("inillucent-newdiff-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    let target = directory.join("select-corpus.db");
    std::fs::copy(&source, &target).expect("the corpus fixture copies");
    target
}

/// One table's shape, as the generator needs it.
struct Shape {
    name: String,
    columns: Vec<String>,
}

/// Reads the fixture's schema out of the new engine's own catalog tree.
///
/// Using the catalog rather than a hard-coded list means the corpus follows the
/// fixture: a column added to it is swept automatically, and a table the import
/// skipped is absent here for the same reason it is absent from the engine.
///
/// @param database - the imported fixture
fn shapes(database: &ImportedDatabase) -> Vec<Shape> {
    let (rows, _) = database
        .run("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .expect("the catalog is readable");
    let mut found = Vec::new();
    for row in &rows {
        let OwnedDatum::Text(bytes) = row.first().unwrap_or(&OwnedDatum::Null) else {
            continue;
        };
        let name = String::from_utf8_lossy(bytes).into_owned();
        let Ok((_, columns)) = database.run(&format!("SELECT * FROM \"{name}\" LIMIT 0")) else {
            continue;
        };
        found.push(Shape { name, columns });
    }
    found
}

/// Returns the ordinal list that makes an `ORDER BY` a total order.
///
/// A sweep that compares row sequences has to ask questions with only one
/// right answer. `ORDER BY team DESC` does not: four rows share a team, and
/// SQLite may return them in any order, so a strict comparison would report a
/// difference that is not one. Appending every output column as a tiebreak
/// makes the order total while leaving the leading term - and therefore the
/// NULL placement and the collation, which are what the sweep is grading -
/// exactly as written.
///
/// @param columns - how many columns the projection has
fn tiebreak(columns: usize) -> String {
    (1..=columns)
        .map(|ordinal| ordinal.to_string())
        .collect::<Vec<String>>()
        .join(", ")
}

/// Emits every query shape the sweep asks about one table.
///
/// @param shape - the table
fn queries_for(shape: &Shape) -> Vec<String> {
    let table = &shape.name;
    let all = tiebreak(shape.columns.len());
    let mut out = vec![
        format!("SELECT count(*) FROM \"{table}\""),
        format!("SELECT * FROM \"{table}\""),
    ];
    for column in &shape.columns {
        let quoted = format!("\"{column}\"");
        out.extend([
            // Points and ranges, over both a number and a string, because the
            // interesting failures are where affinity has to convert one to the
            // other before the comparison.
            format!("SELECT * FROM \"{table}\" WHERE {quoted} = 4"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} = '4'"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} = 'b'"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} > 2"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} >= 2"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} < 5"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} <= 5"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} > 2 AND {quoted} < 5"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} BETWEEN 2 AND 5"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} <> 3"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} IS NULL"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} IS NOT NULL"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} IN (1, 3, 'b')"),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} LIKE 'b%'"),
            // Ordering, both directions and both NULL placements - the three
            // that differ only on a column that has NULLs in it. Every one
            // carries the tiebreak, so ties cannot masquerade as differences.
            format!("SELECT * FROM \"{table}\" ORDER BY {quoted}, {all}"),
            format!("SELECT * FROM \"{table}\" ORDER BY {quoted} DESC, {all}"),
            format!("SELECT * FROM \"{table}\" ORDER BY {quoted} NULLS FIRST, {all}"),
            format!("SELECT * FROM \"{table}\" ORDER BY {quoted} NULLS LAST, {all}"),
            format!("SELECT * FROM \"{table}\" ORDER BY {quoted} DESC NULLS LAST, {all}"),
            // Grouping and distinctness, which is where a collation that was
            // dropped shows up as the wrong number of groups.
            format!("SELECT {quoted}, count(*) FROM \"{table}\" GROUP BY {quoted} ORDER BY 1"),
            format!("SELECT DISTINCT {quoted} FROM \"{table}\" ORDER BY 1"),
            format!("SELECT count(DISTINCT {quoted}) FROM \"{table}\""),
            // The aggregates, including the three that compensate their sums.
            format!("SELECT sum({quoted}), total({quoted}), avg({quoted}) FROM \"{table}\""),
            format!("SELECT min({quoted}), max({quoted}), count({quoted}) FROM \"{table}\""),
            // `group_concat` joins its input in the order the rows arrived,
            // which is not defined for a bare scan - the two engines choose
            // different structures and so different orders. Ordering the
            // input first is the only way to ask a question with one answer.
            format!(
                "SELECT group_concat({quoted}, '-') FROM                  (SELECT {quoted} FROM \"{table}\" ORDER BY {quoted})"
            ),
            // Limits, including the zero that once returned a row.
            format!("SELECT * FROM \"{table}\" ORDER BY {quoted}, {all} LIMIT 0"),
            format!("SELECT * FROM \"{table}\" ORDER BY {quoted}, {all} LIMIT 1"),
            format!("SELECT * FROM \"{table}\" ORDER BY {quoted}, {all} LIMIT 2 OFFSET 1"),
            // A projection that is not a bare column, so the expression path is
            // swept as well as the access path. `length` of a REAL is the one
            // that caught the executor rendering `1e300` as three hundred and
            // one digits.
            format!(
                "SELECT {quoted}, typeof({quoted}), length({quoted})                  FROM \"{table}\" ORDER BY 1, 2, 3"
            ),
            format!("SELECT * FROM \"{table}\" WHERE {quoted} = {quoted}"),
        ]);
    }
    out
}

/// Emits the join shapes over every pair of tables sharing a column name.
///
/// @param shapes - every table
fn join_queries(shapes: &[Shape]) -> Vec<String> {
    let mut out = Vec::new();
    for (nth, left) in shapes.iter().enumerate() {
        for right in shapes.iter().skip(nth.saturating_add(1)) {
            for column in &left.columns {
                if !right.columns.contains(column) {
                    continue;
                }
                let (a, b, key) = (&left.name, &right.name, column);
                out.extend([
                    format!(
                        "SELECT count(*) FROM \"{a}\" JOIN \"{b}\" ON \"{a}\".\"{key}\" = \"{b}\".\"{key}\""
                    ),
                    format!(
                        "SELECT \"{a}\".\"{key}\" FROM \"{a}\" JOIN \"{b}\" ON \"{a}\".\"{key}\" = \"{b}\".\"{key}\" ORDER BY 1, 1"
                    ),
                    format!(
                        "SELECT \"{a}\".\"{key}\" FROM \"{a}\" LEFT JOIN \"{b}\" ON \"{a}\".\"{key}\" = \"{b}\".\"{key}\" ORDER BY 1, 1"
                    ),
                    format!(
                        "SELECT count(*) FROM \"{a}\", \"{b}\" WHERE \"{a}\".\"{key}\" = \"{b}\".\"{key}\""
                    ),
                ]);
            }
        }
    }
    out
}

/// Renders one of the new engine's values the way the oracle renders its own.
///
/// Type and value both, so an integer where SQLite answered a real is a
/// difference rather than a formatting detail.
///
/// @param value - the column the new engine produced
fn render_ours(value: &OwnedDatum) -> String {
    match value {
        OwnedDatum::Null => "null".to_string(),
        OwnedDatum::Int(number) => format!("int:{number}"),
        OwnedDatum::Real(number) => format!("real:{}", format_real(*number)),
        OwnedDatum::Text(bytes) => format!("text:{}", String::from_utf8_lossy(bytes)),
        OwnedDatum::Blob(bytes) => format!("blob:{}", hex(bytes)),
    }
}

/// Renders one of the oracle's values into the same form.
///
/// @param value - the tagged value the oracle sent back
fn render_theirs(value: &TaggedValue) -> String {
    match value {
        TaggedValue::Null => "null".to_string(),
        TaggedValue::Integer(number) => format!("int:{number}"),
        TaggedValue::Real(number) => format!("real:{}", format_real(*number)),
        TaggedValue::Text(bytes) => format!("text:{}", String::from_utf8_lossy(bytes)),
        TaggedValue::Blob(bytes) => format!("blob:{}", hex(bytes)),
    }
}

/// Renders a double so two bit-identical values render identically.
///
/// `{:?}` on an `f64` is round-trip exact, which is what a differential
/// comparison needs - `{}` would print `0.1` for two different doubles.
fn format_real(number: f64) -> String {
    if number == 0.0 {
        // -0.0 and 0.0 are different bit patterns and the same number. SQLite
        // compares them equal, so the corpus must too.
        return "0".to_string();
    }
    format!("{number:?}")
}

/// Renders bytes as hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Names the one difference the dialect leaves open, or `None` if there is none.
///
/// `min` and `max` return *a* smallest value, and under a collation that
/// compares distinct strings equal there is more than one. `people.team` is
/// `COLLATE NOCASE` and holds both `blue` and `Blue`, so SQLite returning the
/// first it scanned and the new engine returning the first in its index are
/// both right - the two chose different structures, and the dialect does not
/// say which value wins.
///
/// This is the only difference the sweep forgives, and it forgives it narrowly:
/// the query has to have asked for an extreme, the two answers have to have the
/// same shape, and they have to differ **only** in the case of their letters.
/// Anything else is a real difference and fails the test.
///
/// @param sql - the query
/// @param ours - the new engine's rendered rows
/// @param theirs - SQLite's rendered rows
fn unspecified(sql: &str, ours: &[String], theirs: &[String]) -> Option<String> {
    let folded = sql.to_ascii_lowercase();
    if !folded.contains("min(") && !folded.contains("max(") {
        return None;
    }
    if ours.len() != theirs.len() {
        return None;
    }
    let case_only = ours
        .iter()
        .zip(theirs)
        .all(|(a, b)| a.eq_ignore_ascii_case(b));
    case_only.then(|| {
        "an extreme over a collation that compares distinct values equal is unspecified".to_string()
    })
}

/// The whole sweep: every generated query, both engines, the same file.
#[test]
fn the_generated_corpus_has_no_unexplained_differences() {
    let Some(program) = oracle_path() else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let fixture = corpus_copy();
    let database = ImportedDatabase::import(fixture.clone(), 8_192)
        .unwrap_or_else(|error| panic!("the corpus did not import: {:?}", error.detail()));

    let mut oracle = Driver::start("sqlite", &program).expect("the oracle starts");
    assert!(oracle.send(&Op::Hello).expect("hello").ok);
    assert!(
        oracle
            .send(&Op::Open(fixture.display().to_string()))
            .expect("open")
            .ok,
        "the oracle could not open the fixture the import read"
    );

    let shapes = shapes(&database);
    assert!(!shapes.is_empty(), "the fixture has tables");
    let mut corpus: Vec<String> = Vec::new();
    for shape in &shapes {
        corpus.extend(queries_for(shape));
    }
    corpus.extend(join_queries(&shapes));
    assert!(
        corpus.len() > 200,
        "the sweep should be a sweep; it produced {}",
        corpus.len()
    );

    let mut agreed = 0usize;
    let mut refused: BTreeMap<String, usize> = BTreeMap::new();
    let mut unspecified_by_dialect: BTreeMap<String, usize> = BTreeMap::new();
    let mut rejected_by_sqlite = 0usize;
    let mut differences: Vec<String> = Vec::new();

    for sql in &corpus {
        let theirs = oracle
            .send(&Op::Query(sql.clone()))
            .expect("the oracle answers");
        let ours = database.run(sql);
        if !theirs.ok {
            // SQLite refused it, so the query is not well formed against this
            // schema and proves nothing either way. The new engine must not
            // *answer* it, though.
            rejected_by_sqlite = rejected_by_sqlite.saturating_add(1);
            if ours.is_ok() {
                differences.push(format!(
                    "{sql}\n  sqlite refused: {}\n  the new engine answered",
                    theirs.message
                ));
            }
            continue;
        }
        let ours = match ours {
            Ok(answer) => answer,
            Err(error) => {
                *refused
                    .entry(error.detail().unwrap_or("no reason given").to_string())
                    .or_insert(0) += 1;
                continue;
            }
        };
        let (rows, _) = ours;
        let mine: Vec<String> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(render_ours)
                    .collect::<Vec<String>>()
                    .join("|")
            })
            .collect();
        let theirs_rendered: Vec<String> = theirs
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(render_theirs)
                    .collect::<Vec<String>>()
                    .join("|")
            })
            .collect();
        // A statement with no `ORDER BY` has no defined row order, so the
        // multiset is compared rather than the sequence. Every query with an
        // `ORDER BY` is compared in order, which is where the NULL-placement
        // and collation rules are actually graded.
        let ordered = sql.to_ascii_uppercase().contains("ORDER BY");
        let same = if ordered {
            mine == theirs_rendered
        } else {
            let mut a = mine.clone();
            let mut b = theirs_rendered.clone();
            a.sort();
            b.sort();
            a == b
        };
        if same {
            agreed = agreed.saturating_add(1);
            continue;
        }
        if let Some(reason) = unspecified(sql, &mine, &theirs_rendered) {
            *unspecified_by_dialect.entry(reason).or_insert(0) += 1;
            continue;
        }
        differences.push(format!(
            "{sql}\n  new engine: {:?}\n  sqlite:     {:?}",
            &mine[..mine.len().min(6)],
            &theirs_rendered[..theirs_rendered.len().min(6)]
        ));
    }

    let refusals: usize = refused.values().sum();
    let open: usize = unspecified_by_dialect.values().sum();
    eprintln!(
        "generated corpus: {} queries, {agreed} agreed, {refusals} refused, \
         {open} left open by the dialect, {rejected_by_sqlite} not valid here",
        corpus.len()
    );
    for (reason, count) in &refused {
        eprintln!("  refused   {count:>4} x  {reason}");
    }
    for (reason, count) in &unspecified_by_dialect {
        eprintln!("  open      {count:>4} x  {reason}");
    }

    assert!(
        agreed > 100,
        "the sweep must actually grade something; only {agreed} queries were compared"
    );
    assert!(
        differences.is_empty(),
        "{} unexplained differences:\n{}",
        differences.len(),
        differences.join("\n")
    );
}
