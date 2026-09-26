//! Turning the corpora that already exist into Layer 1 case files.
//!
//! Invariant: **a converted case asks the same questions its source asked, in
//! the same order, and stops where its source stopped.** The part 8 corpus ran
//! each script with `-bail`, so a converted case ends at the first statement
//! SQLite refuses. Whether a statement is a query, and whether it must succeed,
//! is decided by asking the pinned SQLite at conversion time rather than by
//! reading the text, because a script holds statements the engine under test
//! refuses and the classification must not depend on the thing under test.
//!
//! Conversion is run once, by `inillucent-matrix convert-*`, and its output is
//! checked in. The files are then the cases; nothing re-reads the sources.

use std::fmt::Write as _;
use std::path::Path;

use crate::oracle::{Driver, Op, TaggedValue};
use crate::statement_matrix::case::{
    render_record, split_statements, total_order, Case, Expect, Record, Sort,
};

/// One script to convert: an id, the family it belongs to, and its SQL.
#[derive(Clone, Debug)]
pub struct Source {
    /// The case id, kept from the source so `known.list` lines carry over.
    pub id: String,
    /// The family it is filed under.
    pub family: String,
    /// The capability rows it exercises, when the source said.
    pub capabilities: Vec<String>,
    /// The statements.
    pub script: String,
}

/// Converts one script into records by running it on the oracle.
///
/// @param oracle - a running oracle process
/// @param scratch - a directory for the throwaway database
/// @param source - the script
pub fn convert(oracle: &mut Driver, scratch: &Path, source: &Source) -> Result<Case, String> {
    let path = scratch.join("convert.db");
    inillucent_base::testing::remove_database(&path);
    let opened = oracle.send(&Op::Open(path.display().to_string()))?;
    if !opened.ok {
        return Err(format!("the oracle could not open {}", path.display()));
    }
    let mut case = Case::new(&source.family, "converted");
    case.id = source.id.clone();
    case.capabilities = source.capabilities.clone();
    case.capabilities.extend(capabilities_of(&case.id));
    let place = scratch.join("convert-scratch");
    let _ = std::fs::remove_dir_all(&place);
    let _ = std::fs::create_dir_all(&place);
    let place = place.display().to_string().replace('\\', "/");
    for sql in split_statements(&source.script) {
        let sql = file_names_in_scratch(&sql);
        let local = sql.replace("%SCRATCH%", &place);
        let local = crate::statement_matrix::limited::rewrite(&local).unwrap_or(local);
        let observed = oracle.send(&Op::Query(local))?;
        if !observed.ok {
            case.records.push(Record::Statement {
                expect: Expect::Error(None),
                sql,
            });
            break;
        }
        if observed.columns.is_empty() || !asks_for_rows(&sql) {
            case.records.push(Record::ok(sql));
            continue;
        }
        let sort = if total_order(&sql) {
            Sort::NoSort
        } else {
            Sort::RowSort
        };
        case.records.push(Record::Query {
            types: type_letters(&observed.columns, &observed.rows),
            sort,
            expected: None,
            sql,
        });
    }
    let closed = oracle.send(&Op::Close)?;
    if !closed.ok {
        return Err("the oracle would not close".to_string());
    }
    inillucent_base::testing::remove_database(&path);
    Ok(case)
}

/// Whether a statement is one whose rows the case compares: a query, a
/// `PRAGMA`, an `EXPLAIN`, or a write with `RETURNING`.
///
/// SQLite reports result columns for some statements that are not asking a
/// question: an `ALTER TABLE ... ADD COLUMN ... NOT NULL` compiles a check that
/// has one. Filing that as a query compared the check's column name, which is
/// an artifact of how SQLite implements the statement.
///
/// @param sql - one statement
fn asks_for_rows(sql: &str) -> bool {
    let upper = sql.trim_start().to_ascii_uppercase();
    let first = upper.split_whitespace().next().unwrap_or("");
    matches!(first, "SELECT" | "VALUES" | "WITH" | "PRAGMA" | "EXPLAIN")
        || !crate::statement_matrix::case::top_level_spans(sql, "RETURNING").is_empty()
}

/// The sqllogictest type letters for a result: `I` for an integer, `R` for a
/// real and `T` for anything else, read from the first row.
fn type_letters(columns: &[String], rows: &[Vec<TaggedValue>]) -> String {
    let first = rows.first();
    (0..columns.len().max(1))
        .map(|index| match first.and_then(|row| row.get(index)) {
            Some(TaggedValue::Integer(_)) => 'I',
            Some(TaggedValue::Real(_)) => 'R',
            _ => 'T',
        })
        .collect()
}

/// Renders a family's converted cases as one file, with a header naming where
/// they came from.
///
/// @param header - comment lines, without the `#`
/// @param cases - the cases
pub fn render_file(header: &[&str], cases: &[Case]) -> String {
    let mut out = String::new();
    for line in header {
        let _ = writeln!(out, "# {line}");
    }
    out.push('\n');
    let mut setup: &[Record] = &[];
    for case in cases {
        if case.setup.as_slice() != setup {
            out.push_str("setup\n\n");
            for record in &case.setup {
                render_record(&mut out, record);
            }
            setup = &case.setup;
        }
        let _ = writeln!(out, "case {}", case.id);
        for capability in &case.capabilities {
            let _ = writeln!(out, "capability {capability}");
        }
        out.push('\n');
        for record in &case.records {
            render_record(&mut out, record);
        }
    }
    out
}

/// Moves a case's leading schema and data statements into its setup, so the
/// cases that build the same tables share one fixture.
///
/// Only a prefix moves, and only statements whose effect survives a reopen:
/// `CREATE TABLE`, `CREATE INDEX`, `CREATE VIEW`, `CREATE TRIGGER` that are not
/// `TEMP`, and `INSERT`. Nothing moves when the rest of the case reads a
/// counter the reopen resets (`changes()`, `total_changes()`,
/// `last_insert_rowid()`), or when nothing would be left. The moved statements
/// are still graded, once, when the fixture is built, and the fixture is
/// reopened and checked with `PRAGMA integrity_check` before any case reads it.
///
/// @param case - a converted case with no setup yet
pub fn hoist(case: &mut Case) {
    let prefix = case
        .records
        .iter()
        .take_while(|record| match record {
            Record::Statement {
                expect: Expect::Ok,
                sql,
            } => hoistable(sql),
            _ => false,
        })
        .count();
    if prefix == 0 || prefix >= case.records.len() {
        return;
    }
    let rest_reads_counters = case.records.iter().skip(prefix).any(|record| {
        record.sql().is_some_and(|sql| {
            let lower = sql.to_ascii_lowercase();
            ["changes()", "last_insert_rowid()", "total_changes()"]
                .iter()
                .any(|counter| lower.contains(counter))
        })
    });
    if rest_reads_counters {
        return;
    }
    case.setup = case.records.drain(..prefix).collect();
}

/// Whether a statement's whole effect is in the file, so running it in a
/// fixture instead of in the case changes nothing the case can see.
fn hoistable(sql: &str) -> bool {
    let upper = sql.trim_start().to_ascii_uppercase();
    let words: Vec<&str> = upper.split_whitespace().take(3).collect();
    match words.as_slice() {
        ["INSERT", "INTO", ..] => true,
        ["CREATE", second, ..] => {
            matches!(*second, "TABLE" | "INDEX" | "UNIQUE" | "VIEW" | "TRIGGER")
        }
        _ => false,
    }
}

/// Chooses the family a mixed regression case belongs to, from what its
/// statements do.
///
/// The part 8 corpus files named after tickets (`task2083`, `todo`) hold
/// whatever a ticket found, so their cases are filed by content: the first
/// rule below that matches the script decides.
///
/// @param script - the case's statements
pub fn family_by_content(script: &str) -> &'static str {
    let upper = script.to_ascii_uppercase();
    let rules: &[(&str, &str)] = &[
        ("CREATE TRIGGER", "trigger"),
        ("VIRTUAL TABLE", "vtab"),
        ("JSON_EACH", "vtab"),
        ("GENERATE_SERIES", "vtab"),
        (" OVER", "window"),
        ("REFERENCES", "constraint"),
        ("ON CONFLICT", "insert"),
        ("RETURNING", "insert"),
        ("UPDATE ", "update"),
        ("DELETE ", "delete"),
        ("REINDEX", "maintenance"),
        ("ANALYZE", "maintenance"),
        ("VACUUM", "maintenance"),
        ("CREATE VIEW", "ddl_view"),
        ("CREATE INDEX", "ddl_index"),
        ("CREATE UNIQUE INDEX", "ddl_index"),
        ("WITH ", "cte"),
        (" JOIN ", "join"),
        ("UNION", "compound"),
        ("INTERSECT", "compound"),
        ("EXCEPT", "compound"),
        ("JSON", "function"),
        ("(SELECT", "subquery"),
        ("PRAGMA", "pragma"),
        ("ALTER TABLE", "ddl_table"),
    ];
    rules
        .iter()
        .find(|(needle, _)| upper.contains(needle))
        .map(|(_, family)| *family)
        .unwrap_or("select")
}

/// The family each part 8 category file belongs to, or `None` for the files
/// named after tickets, whose cases are filed by [`family_by_content`].
///
/// @param category - the file's stem
pub fn family_of_category(category: &str) -> Option<&'static str> {
    Some(match category {
        "join" => "join",
        "cte" => "cte",
        "window" | "window2" => "window",
        "compound" | "uniondup" => "compound",
        "subquery" => "subquery",
        "aggregate" | "groupby" | "distinct" | "minmax" | "limit" | "null" | "misc" | "misc2" => {
            "select"
        }
        "affinity" | "affinity2" | "coerce" | "numeric" | "round" | "like" | "collation"
        | "collate2" => "expression",
        "string" | "math" | "datetime" | "datetime2" | "json" => "function",
        "alter" | "check" | "default" | "generated" | "strict" | "withoutrowid" | "schema"
        | "rowid" | "rowid2" => "ddl_table",
        "index" | "index2" => "ddl_index",
        "view" => "ddl_view",
        "fk" | "fk2" => "constraint",
        "trigger" => "trigger",
        "transaction" => "transaction",
        "upsert" | "returning" => "insert",
        "pragma" => "pragma",
        _ => return None,
    })
}

/// The capability rows a converted case exercises, for the cases whose
/// construct the engine has not built.
///
/// A case that meets an `unsupported` answer where SQLite answers passes only
/// when it names a capability row that says `no` or `partial` (section 6.2 of
/// the design), so each converted case that reaches a documented gap names its
/// row here. A gap with no row is a defect in the capability table and goes on
/// the board instead.
pub const CASE_CAPABILITIES: &[(&str, &str)] = &[
    ("lim-004", "computed_limit"),
    ("lim-005", "computed_limit"),
    ("sub-010", "row_value_in_subquery"),
    ("syntax-attach-stmt-p2", "attach_with_key"),
];

/// The capability rows [`CASE_CAPABILITIES`] names for one case.
///
/// @param id - the case id
pub fn capabilities_of(id: &str) -> Vec<String> {
    CASE_CAPABILITIES
        .iter()
        .filter(|(case, _)| *case == id)
        .map(|(_, row)| (*row).to_string())
        .collect()
}

/// Reads the feature probe's cases, exported one per line as the id, a tab,
/// the family, a tab, and the script with `\n` for a newline.
///
/// @param path - the export
pub fn read_probe_export(path: &Path) -> Result<Vec<Source>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut sources = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.splitn(3, '\t');
        let id = fields.next().unwrap_or("").to_string();
        let family = fields.next().unwrap_or("").to_string();
        let script = fields
            .next()
            .unwrap_or("")
            .replace("\\n", "\n")
            .replace("\\\\", "\\");
        if id.is_empty() || family.is_empty() || script.trim().is_empty() {
            return Err(format!(
                "{}: a line needs an id, a family and a script",
                path.display()
            ));
        }
        sources.push(Source {
            id,
            family,
            capabilities: Vec::new(),
            script,
        });
    }
    Ok(sources)
}

/// The schema every syntax register example runs against: enough objects that
/// each example refers to something that exists, so it runs rather than only
/// parsing.
pub const SYNTAX_SETUP: &[&str] = &[
    "CREATE TABLE t(id INTEGER PRIMARY KEY, a, b, c)",
    "INSERT INTO t VALUES(1, 1, 2, 3), (2, 2, 3, 4), (3, NULL, 'x', x'00')",
    "CREATE TABLE u(id INTEGER PRIMARY KEY, a, b UNIQUE, c)",
    "INSERT INTO u VALUES(1, 5, 6, 7), (2, 6, 7, 8)",
    "CREATE TABLE a(x, y)",
    "INSERT INTO a VALUES(1, 2), (2, 3)",
    "CREATE TABLE b(x, y)",
    "INSERT INTO b VALUES(2, 3), (4, 5)",
    "CREATE TABLE c(x, y)",
    "INSERT INTO c VALUES(1, 1)",
    "CREATE VIEW v AS SELECT a, b, c FROM t",
    "CREATE INDEX i ON t(a)",
    "CREATE TRIGGER tr AFTER INSERT ON u BEGIN SELECT 1; END",
];

/// The family a syntax production's examples are filed under.
///
/// @param production - the production's name on the SQLite site
pub fn family_of_production(production: &str) -> &'static str {
    match production {
        "alter-table-stmt" | "create-table-stmt" | "drop-table-stmt" | "column-def"
        | "column-constraint" | "table-constraint" | "conflict-clause" => "ddl_table",
        "analyze-stmt" | "reindex-stmt" | "vacuum-stmt" | "explain-stmt" => "maintenance",
        "attach-stmt" | "detach-stmt" => "schema",
        "begin-stmt" | "commit-stmt" | "rollback-stmt" | "savepoint-stmt" | "release-stmt" => {
            "transaction"
        }
        "create-index-stmt" | "drop-index-stmt" | "indexed-column" => "ddl_index",
        "foreign-key-clause" => "constraint",
        "create-trigger-stmt" | "drop-trigger-stmt" | "raise-function" => "trigger",
        "create-view-stmt" | "drop-view-stmt" => "ddl_view",
        "create-virtual-table-stmt" => "vtab",
        "delete-stmt" | "delete-stmt-limited" | "qualified-table-name" => "delete",
        "insert-stmt" | "upsert-clause" | "returning-clause" => "insert",
        "pragma-stmt" | "signed-number" => "pragma",
        "update-stmt" | "update-stmt-limited" => "update",
        "compound-operator" => "compound",
        "common-table-expression" => "cte",
        "filter-clause"
        | "frame-spec"
        | "over-clause"
        | "window-defn"
        | "window-function-invocation" => "window",
        "join-clause" | "join-constraint" => "join",
        "expr"
        | "literal-value"
        | "numeric-literal"
        | "quoted-identifier"
        | "bind-parameter-tcl-suffix"
        | "type-name" => "expression",
        _ => "select",
    }
}

/// Statements a production's examples need run first: an open transaction
/// for `COMMIT`, a savepoint for `RELEASE`, and the old object dropped for a
/// `CREATE` that reuses its name.
///
/// @param production - the production's name
fn prelude(production: &str) -> &'static [&'static str] {
    match production {
        "commit-stmt" => &["BEGIN"],
        "rollback-stmt" => &["BEGIN", "SAVEPOINT s"],
        "release-stmt" => &["SAVEPOINT s"],
        "detach-stmt" => &["ATTACH '%SCRATCH%/other.db' AS other"],
        "create-table-stmt"
        | "column-def"
        | "column-constraint"
        | "table-constraint"
        | "foreign-key-clause"
        | "conflict-clause"
        | "create-virtual-table-stmt" => &["DROP VIEW v", "DROP TABLE t"],
        "create-index-stmt" | "indexed-column" => &["DROP INDEX i"],
        "create-view-stmt" => &["DROP VIEW v"],
        _ => &[],
    }
}

/// Rewrites a file name in an example so each engine writes under its own
/// scratch directory rather than into the working directory, where the two
/// engines, and two cases, would collide.
///
/// @param sql - the example
pub fn scratch_paths(sql: &str) -> String {
    let named = file_names_in_scratch(sql);
    // A recursive CTE with no bound in the register exists to be parsed and
    // never ends when it is run, so the runnable form stops it after five rows.
    let upper = named.to_ascii_uppercase();
    if upper.starts_with("WITH RECURSIVE") && !upper.contains("LIMIT") {
        return format!("{named} LIMIT 5");
    }
    named
}

/// Puts every bare database file name a statement quotes, such as
/// `'other.db'`, under `%SCRATCH%/`.
///
/// A relative name is resolved against the process's working directory, which
/// both engines share, so an `ATTACH 'd0.db'` in a case would have the two
/// engines, and every case that says the same, writing one file in the
/// checkout.
///
/// @param sql - one statement
pub fn file_names_in_scratch(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(open) = rest.find('\'') {
        let (before, after) = rest.split_at(open);
        out.push_str(before);
        let body = after.get(1..).unwrap_or("");
        let Some(close) = body.find('\'') else {
            out.push_str(after);
            return out;
        };
        let literal = body.get(..close).unwrap_or("");
        let is_file = [".db", ".rdb", ".sqlite"]
            .iter()
            .any(|suffix| literal.ends_with(suffix))
            && !literal.contains(['/', '\\', ':', ' '])
            && !literal.is_empty();
        if is_file {
            out.push_str("'%SCRATCH%/");
        } else {
            out.push('\'');
        }
        out.push_str(literal);
        out.push('\'');
        rest = body.get(close.saturating_add(1)..).unwrap_or("");
    }
    out.push_str(rest);
    out
}

/// Converts every example in the syntax register into cases, one file per
/// family. Returns the family, the file's text and how many cases it holds.
///
/// A positive example becomes a case that runs it after the shared setup and
/// its production's prelude; a negative one a case that expects it refused.
/// Whether each one succeeds on the pinned SQLite is asked at conversion time,
/// because an example that parses may still be refused for what it names.
///
/// @param oracle - a running oracle process
/// @param scratch - a directory for the throwaway database
/// @param register - the syntax register
pub fn syntax_cases(
    oracle: &mut Driver,
    scratch: &Path,
    register: &crate::syntax::SyntaxRegister,
) -> Result<Vec<(String, String, usize)>, String> {
    let mut by_family: std::collections::BTreeMap<&'static str, Vec<Case>> =
        std::collections::BTreeMap::new();
    for production in &register.productions {
        let family = family_of_production(&production.name);
        let examples = production
            .positive
            .iter()
            .map(|sql| (sql, true))
            .chain(production.negative.iter().map(|sql| (sql, false)));
        for (number, (example, positive)) in examples.enumerate() {
            if example.trim().is_empty() {
                continue;
            }
            if std::env::var_os("INILLUCENT_MATRIX_TRACE").is_some() {
                eprintln!("converting {} {number}: {example}", production.name);
            }
            let id = format!(
                "syntax-{}-{}{}",
                production.name,
                if positive { "p" } else { "n" },
                number
            );
            let case = syntax_case(oracle, scratch, &production.name, family, &id, example)?;
            by_family.entry(family).or_default().push(case);
        }
    }
    let mut files = Vec::new();
    for (family, cases) in by_family {
        let mut text = String::new();
        for line in [
            "# Every example in compat/syntax.toml, run rather than only parsed. The setup below",
            "# is shared by every case in the file; a case's first records are its production's",
            "# prelude. Written by `inillucent-matrix convert-syntax`.",
            "",
        ] {
            text.push_str(line);
            text.push('\n');
        }
        for sql in SYNTAX_SETUP {
            text.push_str("statement ok\n");
            text.push_str(sql);
            text.push_str("\n\n");
        }
        let count = cases.len();
        text.push_str(render_file(&[], &cases).trim_start());
        files.push((family.to_string(), text, count));
    }
    Ok(files)
}

/// Converts one syntax example into a case, asking the oracle what it does.
fn syntax_case(
    oracle: &mut Driver,
    scratch: &Path,
    production: &str,
    family: &str,
    id: &str,
    example: &str,
) -> Result<Case, String> {
    let path = scratch.join("syntax.db");
    inillucent_base::testing::remove_database(&path);
    let directory = scratch.join("syntax-scratch");
    let _ = std::fs::create_dir_all(&directory);
    let opened = oracle.send(&Op::Open(path.display().to_string()))?;
    if !opened.ok {
        return Err(format!("the oracle could not open {}", path.display()));
    }
    for sql in SYNTAX_SETUP {
        let done = oracle.send(&Op::Exec((*sql).to_string()))?;
        if !done.ok {
            return Err(format!(
                "the syntax setup failed on SQLite at `{sql}`: {}",
                done.message
            ));
        }
    }
    let place = directory.display().to_string().replace('\\', "/");
    let local = |sql: &str| sql.replace("%SCRATCH%", &place);
    let mut case = Case::new(family, "compat/syntax.toml");
    case.id = id.to_string();
    case.capabilities.extend(capabilities_of(id));
    for sql in prelude(production) {
        let done = oracle.send(&Op::Exec(local(sql)))?;
        case.records.push(if done.ok {
            Record::ok(*sql)
        } else {
            Record::Statement {
                expect: Expect::Error(None),
                sql: (*sql).to_string(),
            }
        });
    }
    let sql = scratch_paths(example);
    let single = split_statements(&sql).len() == 1;
    let observed = if single {
        let asked = local(&sql);
        oracle.send(&Op::Query(
            crate::statement_matrix::limited::rewrite(&asked).unwrap_or(asked),
        ))?
    } else {
        oracle.send(&Op::Exec(local(&sql)))?
    };
    let record = if !observed.ok {
        Record::Statement {
            expect: Expect::Error(None),
            sql,
        }
    } else if single && !observed.columns.is_empty() && asks_for_rows(&sql) {
        Record::Query {
            types: type_letters(&observed.columns, &observed.rows),
            sort: if total_order(&sql) {
                Sort::NoSort
            } else {
                Sort::RowSort
            },
            expected: None,
            sql,
        }
    } else {
        Record::ok(sql)
    };
    case.records.push(record);
    let closed = oracle.send(&Op::Close)?;
    if !closed.ok {
        return Err("the oracle would not close".to_string());
    }
    inillucent_base::testing::remove_database(&path);
    let _ = std::fs::remove_dir_all(&directory);
    Ok(case)
}
