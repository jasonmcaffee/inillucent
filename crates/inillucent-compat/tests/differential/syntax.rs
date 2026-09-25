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

use inillucent_base::limits::{Limit, Limits};
use inillucent_compat::oracle::{Driver, Op};
use inillucent_compat::syntax::{self, SyntaxRegister, SyntaxStatus};
use inillucent_compat::workspace_root;
use inillucent_sql::parser::{self, StatementClass};

/// Loads the shipped register.
fn register() -> SyntaxRegister {
    SyntaxRegister::load(&workspace_root().join("compat/syntax.toml")).expect("the register parses")
}

/// Parses one statement with the default limits.
fn parse(sql: &str) -> Result<parser::ParsedStatement, inillucent_sql::ParseError> {
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
    // **`ParserDepth`, not `ExprDepth`.** The two were one number until they
    // were split: the parser's own recursion is charged to the first
    // and the depth of the expression *tree* to the second, because a redundant
    // parenthesis is a level of one and not of the other. This test is about
    // the parser refusing rather than growing a frame per nesting level, so it
    // is the parser's own limit it lowers.
    let mut limits = Limits::default();
    limits.set(Limit::ParserDepth, 100);
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

/// H8 (task-1920): a flat chain is charged against `ExprDepth`, which was
/// declared and enforced nowhere.
///
/// **Why a flat chain and not a nest.** `adversarial_depth_is_refused_rather_than_crashing`
/// above lowers `ParserDepth` and nests parentheses, which charges the
/// parser's own recursion. `a1 = 1 AND a2 = 2 AND ...` charges nothing at all:
/// the Pratt loop enters and leaves `parse_expr_bp` once per term, so the
/// recursion counter never accumulates, while the tree grows one level per
/// term because `AND` is left-associative. `compat/limits.toml` declares
/// `ExprDepth` with a default of 1000 - SQLite's - and nothing read it.
///
/// A tree that deep is not a parser problem on its own. It is a problem for
/// the binder, the planner and the executor, each of which walks it
/// recursively, and the `SqlLength` default of 1 GiB leaves room for a chain
/// tens of millions of terms long.
///
/// The last case is the one that says the limit is charged as the tree grows
/// rather than after it is built: at the default `ExprDepth` of 1000 a chain
/// of 200,000 terms is refused, and it is refused quickly.
#[test]
fn a_flat_chain_is_charged_against_the_expression_depth_limit() {
    let mut limits = Limits::default();
    limits.set(Limit::ExprDepth, 100);
    let chain: String = (0..200)
        .map(|nth| format!("a{nth} = {nth}"))
        .collect::<Vec<String>>()
        .join(" AND ");
    let sql = format!("SELECT 1 WHERE {chain}");
    let failure = match parser::parse_next_statement(sql.as_bytes(), 0, &limits) {
        Err(failure) => failure,
        Ok(_) => panic!("a 200-term chain must be refused at an ExprDepth of 100"),
    };
    assert!(
        failure.message().contains("depth"),
        "the refusal must name the depth: {}",
        failure.message()
    );

    // A chain inside the limit still parses, which is the half a limit that
    // refused everything would break.
    let short: String = (0..50)
        .map(|nth| format!("a{nth} = {nth}"))
        .collect::<Vec<String>>()
        .join(" AND ");
    parser::parse_next_statement(format!("SELECT 1 WHERE {short}").as_bytes(), 0, &limits)
        .expect("a 50-term chain is inside a limit of 100");

    // And at the shipped default, a chain SQLite refuses is refused here too.
    let long: String = (0..200_000)
        .map(|nth| format!("a{nth} = {nth}"))
        .collect::<Vec<String>>()
        .join(" AND ");
    let started = std::time::Instant::now();
    let refused = parser::parse_next_statement(
        format!("SELECT 1 WHERE {long}").as_bytes(),
        0,
        &Limits::default(),
    );
    assert!(
        refused.is_err(),
        "a 200,000-term chain must be refused at the default ExprDepth of 1000"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the refusal took {:?}, which means the limit is charged after the tree \
         is built rather than as it grows",
        started.elapsed()
    );
}

/// H8 (task-1920): interning N distinct identifiers costs N, not N squared.
///
/// **What it used to cost.** `Ast::intern` walked every name interned so far
/// and compared three fields against each, so a statement naming N distinct
/// identifiers cost N-squared comparisons. Under the 1 GiB `SqlLength` default
/// a statement can name hundreds of thousands of them.
///
/// **A wall-clock bound is a weak assertion and a ratio is not.** Timing one
/// run says nothing on a busy machine; doubling the input and asserting the
/// work does not quadruple says exactly what "quadratic" means. The bound is
/// deliberately loose - four times the smaller run, where the defect would give
/// sixteen - so that this fails on the algorithm rather than on the scheduler.
///
/// **Each size is timed three times and the fastest is kept, and the bound is
/// the four its own sentence above already named (task-2039).** It asserted
/// three, and it timed each size once. Both halves of that were wrong on a
/// shared machine: a single reading carries whatever the scheduler was doing
/// during it, and the two readings are taken at different moments, so the load
/// does not cancel between them. It failed in a 158-target run at 3.27 -
/// 143.5 ms against 43.9 ms - on a change that made interning *faster*. Timed
/// nine times each on an idle box the real figure is 2.0 on both sides of that
/// change: 8,000 identifiers in 1.98 ms and 16,000 in 4.34 ms before it, 1.89
/// and 4.05 after. The fastest of three runs is the reading least contaminated
/// by everything else on the box, and a defect that made this quadratic would
/// be at sixteen, where no amount of load matters.
#[test]
fn interning_distinct_identifiers_is_not_quadratic() {
    let names = |count: usize| -> String {
        let list: Vec<String> = (0..count).map(|nth| format!("c{nth}")).collect();
        format!("SELECT {} FROM t", list.join(", "))
    };
    // The result-set column limit defaults to 2,000 and this is not a test
    // about that limit, so it is raised to its own hard maximum of 32,767 -
    // which is what bounds the two sizes below. The identifier-count limit
    // `charge_expr_depth` applies is derived from it and moves with it.
    let mut limits = Limits::default();
    limits.set(Limit::Column, 32_767);
    let time = |sql: &str| -> std::time::Duration {
        let started = std::time::Instant::now();
        parser::parse_next_statement(sql.as_bytes(), 0, &limits).expect("it parses");
        started.elapsed()
    };
    let fastest_of_three =
        |sql: &str| -> std::time::Duration { (0..3).map(|_| time(sql)).min().unwrap_or_default() };
    // Warm the allocator and the branch predictors so the first run is not the
    // one that pays for them.
    let _ = time(&names(2_000));
    let small = fastest_of_three(&names(8_000));
    let large = fastest_of_three(&names(16_000));
    assert!(
        large
            < small
                .saturating_mul(4)
                .max(std::time::Duration::from_millis(50)),
        "doubling the identifiers took {large:?} against {small:?} for half as \
         many, which is the quadratic scan rather than the map"
    );
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
    assert_eq!(third.statement, inillucent_sql::ast::Statement::Empty);
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
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
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
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
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
                    "{}: `{example}` sqlite={accepted} inillucent={ours}",
                    production.name
                ));
            }
        }
    }
    let _ = driver.send(&Op::Bye);
    assert!(divergences.is_empty(), "{divergences:#?}");
}

/// Statements the pinned release accepts, in which a word that is a keyword
/// stands where a **name** is expected.
///
/// `LEFT`, `RIGHT`, `FULL`, `INNER`, `CROSS`, `NATURAL`, `OUTER` and `INDEXED`
/// are absent from SQLite's `%fallback` declaration and are still legal names,
/// because its name production accepts the `idj` token class directly:
/// `nm ::= idj | STRING` with `idj ::= ID|INDEXED|JOIN_KW`. A diff table, a
/// tree, a stereo channel and a page layout all use `left` and `right`, so this
/// is ordinary SQL rather than an exotic corner.
const NAMES_SQLITE_ACCEPTS: &[&str] = &[
    "CREATE TABLE pairs (left TEXT, right TEXT)",
    "CREATE TABLE sides (full TEXT, inner TEXT, cross TEXT, natural TEXT, outer TEXT)",
    "CREATE TABLE hinted (indexed TEXT)",
    "CREATE TABLE left (a INT)",
    "CREATE TABLE indexed (a INT)",
    "SELECT left, right FROM t",
    "SELECT t.left FROM t",
    "SELECT main.t.left FROM t",
    "SELECT a FROM t AS left",
    "SELECT a FROM t AS indexed",
    "SELECT 1 AS left",
    "SELECT sum(left) FROM t GROUP BY left HAVING left > 0",
    "SELECT left FROM t ORDER BY left",
    "CREATE INDEX left ON t (a)",
    "CREATE VIEW left AS SELECT 1",
    "UPDATE t SET left = 2",
    "INSERT INTO t (left) VALUES (1)",
    "ALTER TABLE t ADD COLUMN left TEXT",
    "ALTER TABLE t RENAME TO left",
    "WITH left AS (SELECT 1) SELECT * FROM left",
    // A fallback keyword IS a type name, because it lexes as `ID`.
    "CREATE TABLE typed (a key)",
    "CREATE TABLE typed (left key)",
];

/// Statements the pinned release **refuses**, and which a careless widening of
/// the name rule would make parse.
///
/// Two positions take `ids` (`ID|STRING`) rather than `idj`: a bare alias and a
/// declared type name. That is not a detail. The alias rule is what stops
/// `FROM t LEFT JOIN u` reading `LEFT` as the alias of `t` and `FROM t INDEXED
/// BY i` reading `INDEXED` as one; the type rule is why `CREATE TABLE t (a
/// left)` is a syntax error in SQLite while `CREATE TABLE t (left TEXT)` is
/// not.
const NARROW_POSITIONS_SQLITE_REFUSES: &[&str] = &[
    // A bare alias, `as ::= ids`.
    "SELECT a left FROM t",
    "SELECT a right FROM t",
    "SELECT a indexed FROM t",
    "SELECT a FROM t left",
    "SELECT a FROM t indexed",
    // A declared type, `typename ::= ids`. The column *name* beside it takes
    // the wide class, which is why `left TEXT` parses and `a left` does not -
    // and why widening one predicate for both positions would have traded one
    // divergence from the pinned release for another.
    "CREATE TABLE typed (a left)",
    "CREATE TABLE typed (a indexed)",
    "CREATE TABLE typed (left left)",
    "CREATE TABLE typed (a unsigned big left)",
];

/// Statements whose join keyword must still be a join keyword.
const JOINS_THAT_MUST_STILL_JOIN: &[&str] = &[
    "SELECT * FROM t LEFT JOIN u ON t.a = u.x",
    "SELECT * FROM t LEFT OUTER JOIN u ON t.a = u.x",
    "SELECT * FROM t INNER JOIN u ON t.a = u.x",
    "SELECT * FROM t CROSS JOIN u",
    "SELECT * FROM t NATURAL JOIN u",
    "SELECT * FROM t INDEXED BY i WHERE b = 'x'",
    "SELECT * FROM t NOT INDEXED WHERE b = 'x'",
    "SELECT a, count(*) OVER w FROM t WINDOW w AS (ORDER BY a)",
];

/// A keyword SQLite allows as a name parses as one here, in every position it
/// allows one.
///
/// This is the ticket-1847 parity gap: `CREATE TABLE pairs (left TEXT, right
/// TEXT)` is a schema SQLite writes and this parser refused, which made an
/// import of an ordinary database fail. The list is deliberately wider than the
/// reproduction, because the rule is per *position* and fixing only the column
/// declaration would have left every other name position broken.
#[test]
fn a_keyword_sqlite_allows_as_a_name_parses_as_one() {
    let mut failures = Vec::new();
    for statement in NAMES_SQLITE_ACCEPTS {
        if let Err(failure) = parse(statement) {
            failures.push(format!("`{statement}` should parse: {}", failure.message()));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The other half, which is the half a flat widening breaks.
///
/// The two `ids` positions take the fallback set and not the token class, so
/// these are syntax errors in the pinned release and must stay syntax errors
/// here. Were the alias half to parse, `FROM t LEFT JOIN u` would silently
/// become a cross join of `t` aliased `left` against a table named `join` - a
/// wrong answer rather than a refusal.
#[test]
fn a_join_keyword_is_still_not_a_bare_alias_or_a_type_name() {
    let mut failures = Vec::new();
    for statement in NARROW_POSITIONS_SQLITE_REFUSES {
        if parse(statement).is_ok() {
            failures.push(format!("`{statement}` should not parse"));
        }
    }
    for statement in JOINS_THAT_MUST_STILL_JOIN {
        if let Err(failure) = parse(statement) {
            failures.push(format!("`{statement}` should parse: {}", failure.message()));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Both lists are put to the pinned release, so the claim about what SQLite
/// does is measured rather than remembered.
#[test]
fn the_reserved_word_lists_agree_with_the_pinned_release() {
    let Some(mut driver) = start_oracle() else {
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let mut divergences = Vec::new();
    let mut compared = 0;
    for (statement, expected) in NAMES_SQLITE_ACCEPTS
        .iter()
        .chain(JOINS_THAT_MUST_STILL_JOIN.iter())
        .map(|statement| (statement, true))
        .chain(
            NARROW_POSITIONS_SQLITE_REFUSES
                .iter()
                .map(|statement| (statement, false)),
        )
    {
        // A statement naming an object the oracle's small schema does not have
        // still answers a *syntax* verdict, which is the only thing compared.
        let Some(accepted) = oracle_syntax_verdict(&mut driver, statement) else {
            continue;
        };
        compared += 1;
        if accepted != expected {
            divergences.push(format!(
                "the list says sqlite={expected} for `{statement}`, the oracle says {accepted}"
            ));
        }
        let ours = parse(&explained(statement)).is_ok();
        if ours != accepted {
            divergences.push(format!("`{statement}` sqlite={accepted} inillucent={ours}"));
        }
    }
    let _ = driver.send(&Op::Bye);
    assert!(compared > 20, "only {compared} statements had a verdict");
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
        inillucent_compat::differential::skipping("the pinned SQLite oracle is not built");
        return;
    };
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let mut divergences = Vec::new();
    // What gets written to the corpus is the statement, never the annotation:
    // a file of "`X` sqlite=false inillucent=true" lines replays as *that* string,
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
            divergences.push(format!("`{sql}` sqlite={accepted} inillucent={ours}"));
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

/// A literal past the length limit is refused without being copied first.
///
/// **The check ran after the copy (task-1932, M8).** `string_text` and
/// `blob_bytes` allocate the decoded value and `charge_literal` then measured
/// what had already been allocated, so a two gigabyte literal in a statement
/// handed to a served connection was copied first and refused second - which is
/// the one case a length limit exists to prevent.
///
/// The bound is now read off the token's span before anything is decoded. Both
/// forms it computes are sound: a blob's decoded length is exactly half its
/// hexadecimal digits, and a quoted string's is at least half of what is
/// between the quotes, because the shortest thing one character can decode from
/// is a doubled quote.
#[test]
fn a_literal_past_the_length_limit_is_refused() {
    let mut limits = Limits::default();
    limits.set(Limit::Length, 64);

    let long = "x".repeat(10_000);
    let sql = format!("SELECT '{long}'");
    let failure = match parser::parse_next_statement(sql.as_bytes(), 0, &limits) {
        Err(failure) => failure,
        Ok(_) => panic!("a 10,000 character literal must be refused at a Length of 64"),
    };
    assert!(
        failure.message().contains("too big"),
        "the refusal must name the size: {}",
        failure.message()
    );

    let blob = format!("SELECT x'{}'", "ab".repeat(10_000));
    assert!(
        parser::parse_next_statement(blob.as_bytes(), 0, &limits).is_err(),
        "a 10,000 byte blob must be refused at a Length of 64"
    );
}

/// A literal inside the limit is still accepted, at every length the span bound
/// could get wrong.
///
/// **The half a lower bound can break.** Refusing on the span rather than on
/// the decoded length is only correct while the bound is a true lower bound: a
/// literal of sixty four doubled quotes is a hundred and thirty bytes of span
/// and sixty four characters decoded, which is exactly at the limit and has to
/// be accepted. A bound that was not a lower bound would refuse a statement
/// SQLite answers, which is worse than the defect it replaced.
#[test]
fn a_literal_inside_the_length_limit_is_still_accepted() {
    let mut limits = Limits::default();
    limits.set(Limit::Length, 64);

    for length in [0usize, 1, 32, 63, 64] {
        let text = "a".repeat(length);
        let sql = format!("SELECT '{text}'");
        assert!(
            parser::parse_next_statement(sql.as_bytes(), 0, &limits).is_ok(),
            "a {length} character literal was refused at a Length of 64"
        );
    }

    let doubled = "''".repeat(64);
    let sql = format!("SELECT '{doubled}'");
    assert!(
        parser::parse_next_statement(sql.as_bytes(), 0, &limits).is_ok(),
        "a literal of 64 quote characters was refused at a Length of 64, so the span bound is \
         not a lower bound"
    );

    let inside = format!("SELECT x'{}'", "ff".repeat(64));
    assert!(
        parser::parse_next_statement(inside.as_bytes(), 0, &limits).is_ok(),
        "a 64 byte blob was refused at a Length of 64"
    );
    let outside = format!("SELECT x'{}'", "ff".repeat(65));
    assert!(
        parser::parse_next_statement(outside.as_bytes(), 0, &limits).is_err(),
        "a 65 byte blob was accepted at a Length of 64"
    );
}
