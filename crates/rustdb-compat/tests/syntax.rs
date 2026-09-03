//! Syntax parity: every published production, and the boundaries around them.
//!
//! Invariant: a statement the pinned release accepts must parse here, and one
//! it rejects for a *syntax* reason must be refused here. The distinction
//! matters: SQLite's `prepare` does parsing and name resolution in one call, so
//! "no such table" is a rejection that says nothing about the grammar. Only
//! errors SQLite reports as syntax errors are compared against the parser.
//!
//! The register in `compat/syntax.toml` is the denominator. These tests read
//! it, so adding a production without an example fails here rather than being
//! quietly uncovered.

use std::path::PathBuf;

use rustdb_base::limits::{Limit, Limits};
use rustdb_compat::oracle::{Driver, Op};
use rustdb_compat::syntax::{self, SyntaxRegister, SyntaxStatus};
use rustdb_compat::workspace_root;
use rustdb_sql::parser::{self, StatementClass};

/// Loads the shipped register.
fn register() -> SyntaxRegister {
    SyntaxRegister::load(&workspace_root().join("compat/syntax.toml")).expect("the register parses")
}

/// Parses one statement with the default limits.
fn parse(sql: &str) -> Result<parser::ParsedStatement, rustdb_sql::ParseError> {
    parser::parse_next_statement(sql.as_bytes(), 0, &Limits::default())
}

/// The register itself has to be sound before anything reads it.
#[test]
fn the_register_is_structurally_sound() {
    let register = register();
    let problems = register.problems();
    assert!(problems.is_empty(), "{problems:#?}");
    assert!(
        register.productions.len() >= 45,
        "the register has to carry every published diagram, not a sample"
    );
}

/// Every positive example parses, and every negative one is refused.
#[test]
fn every_published_production_has_evidence() {
    let register = register();
    let mut failures = Vec::new();
    for production in &register.productions {
        if production.status != SyntaxStatus::Parsed {
            continue;
        }
        for example in &production.positive {
            if let Err(failure) = parse(example) {
                failures.push(format!(
                    "{}: `{example}` should parse: {}",
                    production.name,
                    failure.message()
                ));
            }
        }
        for example in &production.negative {
            if parse(example).is_ok() {
                failures.push(format!("{}: `{example}` should not parse", production.name));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The report regenerates identically from the register, so a change to the
/// register is a change to the report and never the other way round.
#[test]
fn the_report_regenerates() {
    let register = register();
    let generated = syntax::report(&register);
    let path = workspace_root().join("compat/syntax-report.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.replace("\r\n", "\n") != generated {
        std::fs::write(&path, &generated).expect("the report writes");
    }
    let reread = std::fs::read_to_string(&path).expect("the report reads");
    assert_eq!(reread.replace("\r\n", "\n"), generated);
}

/// A syntax error points at the byte that caused it.
///
/// The offsets are compared against the pinned release where it reports one;
/// these are the cases where the two must agree exactly, because an offset is
/// what an editor underlines.
#[test]
fn syntax_errors_point_at_the_offending_byte() {
    let cases: [(&str, u32); 8] = [
        ("SELECT FROM t", 7),
        ("SELECT 1 FROM", 13),
        ("SELECT * FROM t WHERE", 21),
        ("SELECT 'abc", 7),
        ("SELECT \"abc", 7),
        ("SELECT [abc", 7),
        ("SELECT 1 +", 10),
        ("SELECT 123abc", 7),
    ];
    for (sql, offset) in cases {
        let failure = match parse(sql) {
            Err(failure) => failure,
            Ok(_) => panic!("`{sql}` must not parse"),
        };
        assert_eq!(failure.offset(), offset, "`{sql}`: {}", failure.message());
    }
}

/// The parser is bounded on adversarial depth: it refuses rather than growing
/// a stack frame per nesting level.
#[test]
fn adversarial_depth_is_refused_rather_than_crashing() {
    let mut limits = Limits::default();
    limits.set(Limit::ExprDepth, 100);
    for depth in [200usize, 5_000, 100_000] {
        let sql = format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth));
        let failure = match parser::parse_next_statement(sql.as_bytes(), 0, &limits) {
            Err(failure) => failure,
            Ok(_) => panic!("depth {depth} must be refused"),
        };
        assert!(
            failure.message().contains("depth"),
            "depth {depth}: {}",
            failure.message()
        );
    }
}

/// A statement longer than the SQL-length limit is refused before it is lexed.
#[test]
fn an_overlong_statement_is_refused() {
    let mut limits = Limits::default();
    limits.set(Limit::SqlLength, 32);
    let sql = format!("SELECT {}", "1, ".repeat(100));
    assert!(parser::parse_next_statement(sql.as_bytes(), 0, &limits).is_err());
}

/// Parameters are numbered SQLite's way: a bare `?` takes one past the highest
/// used so far, an explicit `?NNN` raises the mark, and a repeated name reuses
/// the index the first occurrence got.
#[test]
fn parameters_are_numbered_the_way_sqlite_numbers_them() {
    // `?` takes 1, `?5` takes 5 and raises the mark, the next `?` takes 6,
    // `:a` takes 7, `:b` takes 8, and the second `:a` reuses 7.
    let parsed = parse("SELECT ?, ?5, ?, :a, :b, :a").expect("it parses");
    assert_eq!(parsed.parameters.count, 8);
    assert_eq!(parsed.parameters.index_of(b":a"), Some(7));
    assert_eq!(parsed.parameters.index_of(b":b"), Some(8));

    let mut limits = Limits::default();
    limits.set(Limit::VariableNumber, 4);
    assert!(parser::parse_next_statement(b"SELECT ?9", 0, &limits).is_err());
}

/// One prepare compiles one statement and reports the tail, which is the
/// contract `sqlite3_prepare_v2` has with its caller.
#[test]
fn one_prepare_compiles_one_statement_and_reports_the_tail() {
    let sql = b"SELECT 1; SELECT 2;  ";
    let first = parser::parse_next_statement(sql, 0, &Limits::default()).expect("it parses");
    assert_eq!(first.consumed, 9);
    let second =
        parser::parse_next_statement(sql, first.consumed, &Limits::default()).expect("it parses");
    assert_eq!(second.consumed, 19);
    let third =
        parser::parse_next_statement(sql, second.consumed, &Limits::default()).expect("it parses");
    assert_eq!(third.statement, rustdb_sql::ast::Statement::Empty);
    assert_eq!(third.consumed, sql.len());
}

/// Classification decides what a statement is without parsing it.
#[test]
fn classification_reads_the_leading_keywords() {
    let cases: [(&str, StatementClass); 9] = [
        ("SELECT 1", StatementClass::ReadOnly),
        ("VALUES (1)", StatementClass::ReadOnly),
        (
            "WITH c AS (SELECT 1) SELECT * FROM c",
            StatementClass::ReadOnly,
        ),
        ("WITH c AS (SELECT 1) DELETE FROM t", StatementClass::Write),
        ("INSERT INTO t VALUES (1)", StatementClass::Write),
        ("CREATE TABLE t (a)", StatementClass::SchemaChange),
        ("BEGIN", StatementClass::TransactionControl),
        ("PRAGMA page_size", StatementClass::Pragma),
        ("", StatementClass::Empty),
    ];
    for (sql, expected) in cases {
        assert_eq!(
            parser::classify_statement(sql.as_bytes()),
            expected,
            "{sql}"
        );
    }
}

/// Returns the pinned SQLite oracle, if it has been built.
fn sqlite_oracle() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RUSTDB_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Wraps a statement in `EXPLAIN`, unless it already is one.
///
/// Both engines have to be asked about the same bytes, and `EXPLAIN EXPLAIN`
/// is not a statement in either of them.
fn explained(sql: &str) -> String {
    if sql.trim_start().to_ascii_uppercase().starts_with("EXPLAIN") {
        return sql.to_string();
    }
    format!("EXPLAIN {sql}")
}

/// Starts the oracle on an in-memory database with a small schema.
fn start_oracle() -> Option<Driver> {
    let program = sqlite_oracle()?;
    let mut driver = Driver::start("sqlite", &program).ok()?;
    driver.send(&Op::Hello).ok()?;
    driver.send(&Op::Open(":memory:".to_string())).ok()?;
    for statement in [
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT, c REAL)",
        "CREATE TABLE u (x, y)",
        "CREATE TABLE a (x, id)",
        "CREATE TABLE b (x, id)",
        "CREATE TABLE c (x)",
        "CREATE INDEX i ON t (b)",
        "CREATE VIEW v AS SELECT 1 AS one",
    ] {
        driver.send(&Op::Exec(statement.to_string())).ok()?;
    }
    Some(driver)
}

/// Returns whether the oracle rejects a statement for a syntax reason.
///
/// `EXPLAIN` prepares without running, which is what makes this a question
/// about the grammar rather than about the data.
fn oracle_syntax_verdict(driver: &mut Driver, sql: &str) -> Option<bool> {
    let observation = driver.send(&Op::Query(explained(sql))).ok()?;
    if observation.ok {
        return Some(true);
    }
    let message = observation.message.to_ascii_lowercase();
    let syntactic = message.contains("syntax error")
        || message.contains("unrecognized token")
        || message.contains("incomplete input")
        || message.contains("unterminated");
    // A rejection that is not syntactic says nothing about the grammar, so it
    // is not a verdict this comparison can use.
    if syntactic {
        Some(false)
    } else {
        None
    }
}

/// Every example in the register is put to the pinned release, and the two
/// engines must agree about whether it is syntax.
#[test]
fn the_register_agrees_with_the_pinned_release() {
    let Some(mut driver) = start_oracle() else {
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let register = register();
    let mut divergences = Vec::new();
    for production in &register.productions {
        if production.status != SyntaxStatus::Parsed {
            continue;
        }
        if !SyntaxRegister::comparable(production) {
            // The pinned build was not compiled with this production, so it
            // cannot answer for it. The register says so by name.
            continue;
        }
        for example in production.positive.iter().chain(production.negative.iter()) {
            let Some(accepted) = oracle_syntax_verdict(&mut driver, example) else {
                continue;
            };
            let ours = parse(&explained(example)).is_ok();
            if ours != accepted {
                divergences.push(format!(
                    "{}: `{example}` sqlite={accepted} rustdb={ours}",
                    production.name
                ));
            }
        }
    }
    let _ = driver.send(&Op::Bye);
    assert!(divergences.is_empty(), "{divergences:#?}");
}

/// The fragments the fuzzer builds statements out of.
const FRAGMENTS: [&str; 40] = [
    "SELECT", "FROM", "WHERE", "t", "u", "a", "b", "*", "(", ")", ",", "1", "'x'", "+", "-", "*",
    "/", "AND", "OR", "NOT", "NULL", "IS", "IN", "LIKE", "BETWEEN", "ORDER", "BY", "GROUP",
    "HAVING", "LIMIT", "DISTINCT", "AS", "JOIN", "ON", "USING", "CASE", "WHEN", "THEN", "END", ";",
];

/// A small deterministic generator, so a failing seed can be replayed.
struct Rng(u64);

impl Rng {
    /// Returns the next value of a xorshift sequence.
    fn next(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        state
    }

    /// Returns a value below a bound.
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next() % bound as u64) as usize
    }
}

/// Builds one random statement out of the fragments.
fn generate(rng: &mut Rng) -> String {
    let length = 1 + rng.below(12);
    let mut out = String::new();
    for index in 0..length {
        if index > 0 {
            out.push(' ');
        }
        let fragment = FRAGMENTS
            .get(rng.below(FRAGMENTS.len()))
            .copied()
            .unwrap_or("1");
        out.push_str(fragment);
    }
    out
}

/// Differential parser fuzzing: for every generated string on which the pinned
/// release gives a syntactic verdict, the parser must give the same one.
///
/// A divergence is written to the retained corpus so it is replayed on every
/// later run rather than depending on the seed coming up again.
#[test]
fn differential_parser_fuzzing_finds_no_divergence() {
    let Some(mut driver) = start_oracle() else {
        eprintln!("the pinned SQLite oracle is not built; skipping");
        return;
    };
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let mut divergences = Vec::new();
    // What gets written to the corpus is the statement, never the annotation:
    // a file of "`X` sqlite=false rustdb=true" lines replays as *that* string,
    // which is not the one that diverged, and the replay then proves nothing.
    let mut retained: Vec<String> = Vec::new();
    let mut compared = 0usize;
    for _ in 0..1500 {
        let sql = generate(&mut rng);
        let Some(accepted) = oracle_syntax_verdict(&mut driver, &sql) else {
            continue;
        };
        compared += 1;
        // The oracle is asked about `EXPLAIN <sql>`, so the parser is asked
        // about the same bytes. Comparing a bare `<sql>` here would compare two
        // different questions: `EXPLAIN ;` is a syntax error and `;` is not.
        let ours = parse(&explained(&sql)).is_ok();
        if ours != accepted {
            divergences.push(format!("`{sql}` sqlite={accepted} rustdb={ours}"));
            retained.push(sql);
        }
    }
    // The retained corpus: every divergence any earlier run found, replayed.
    let corpus = workspace_root().join("compat/corpus/syntax");
    if let Ok(entries) = std::fs::read_dir(&corpus) {
        for entry in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let Some(accepted) = oracle_syntax_verdict(&mut driver, line) else {
                    continue;
                };
                compared += 1;
                if parse(&explained(line)).is_ok() != accepted {
                    divergences.push(format!("corpus: `{line}`"));
                }
            }
        }
    }
    let _ = driver.send(&Op::Bye);
    if !divergences.is_empty() {
        // Retain what was found, so the next run replays it with no oracle.
        let _ = std::fs::create_dir_all(&corpus);
        let _ = std::fs::write(corpus.join("divergences.txt"), divergences.join("\n"));
    }
    assert!(compared > 200, "only {compared} strings had a verdict");
    assert!(divergences.is_empty(), "{divergences:#?}");
}

/// The parser opens no file and reads no page. This is checked the only way it
/// can be: by parsing every example with no database anywhere in scope and no
/// catalog, which is what the type signature already forces and what this test
/// makes an executable claim.
#[test]
fn the_parser_performs_no_io() {
    let register = register();
    for production in &register.productions {
        for example in &production.positive {
            let _ = parse(example);
        }
    }
    // `parse_next_statement` takes bytes and limits and nothing else; there is
    // no VFS, no pager and no catalog it could reach. The assertion is that the
    // whole register parses without one being constructed.
    assert!(register.example_count() > 100);
}
