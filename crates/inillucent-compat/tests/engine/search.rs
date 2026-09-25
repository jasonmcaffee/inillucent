//! `inillucent_search`: the retrieval engine as a transactional SQL table.
//!
//! Invariant: a search result is as transactional as a row. An insert that is
//! rolled back leaves no trace in the ranking; an insert that commits is found
//! by the next query; a savepoint takes back exactly what it covered; and a
//! crash leaves the rows and the index agreeing with each other, because they
//! were recovered by the same recovery.
//!
//! There is no SQLite oracle in this file and there should not be. SQLite has
//! no equivalent of this module, and comparing it against FTS5 would be
//! comparing two different retrieval engines and calling the difference a bug.
//! What is checked instead is the contract the module declares: the ordering it
//! promises, the visibility rules, and the equivalence between the SQL door and
//! the direct engine underneath it.

use inillucent_compat::differential::{scratch, start_inillucent};
use inillucent_compat::rendering::datum_text as render;
use inillucent_engine::connect::{Connection, Database};

/// Where this suite's scratch databases live.
const AREA: &str = "search";

/// Runs a statement for its effect.
fn exec(connection: &Connection<'_>, sql: &str) {
    connection
        .execute_batch(sql)
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
}

/// Runs a statement, returning whatever error it produced.
fn try_exec(connection: &Connection<'_>, sql: &str) -> Result<(), String> {
    connection
        .execute_batch(sql)
        .map_err(|error| error.message().to_string())
}

/// Returns every row of a query, each column rendered as text.
fn rows(connection: &Connection<'_>, sql: &str) -> Vec<Vec<String>> {
    let mut statement = connection
        .prepare(sql)
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
    let mut out = Vec::new();
    while statement
        .step()
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()))
    {
        out.push(statement.row().iter().map(render).collect());
    }
    out
}

/// Returns the first column of every row.
fn column(connection: &Connection<'_>, sql: &str) -> Vec<String> {
    rows(connection, sql)
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .collect()
}

/// The corpus every test in this file searches.
///
/// Small and deliberately overlapping: three documents about eligibility, one
/// about something else, and one that shares a word with the first three
/// without answering the same question. That is what makes an ordering
/// meaningful rather than an accident of there being one match.
const CORPUS: &[(i64, &str, &str)] = &[
    (
        1,
        "Offer eligibility",
        "who qualifies for the launch discount offer",
    ),
    (
        2,
        "Discount rules",
        "the discount applies to eligible accounts only",
    ),
    (
        3,
        "Eligible accounts",
        "an account is eligible when it has been open a year",
    ),
    (4, "Weather", "the forecast for tomorrow is rain and wind"),
    (
        5,
        "Launch notes",
        "the launch shipped on a Tuesday with no discount",
    ),
];

/// Creates a lexical-only search table and fills it.
fn seed(connection: &Connection<'_>) {
    exec(
        connection,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body)",
    );
    for (id, title, body) in CORPUS {
        exec(
            connection,
            &format!("INSERT INTO docs(rowid, title, body) VALUES ({id}, '{title}', '{body}')"),
        );
    }
}

/// A search table is created, written and read like any other table.
#[test]
fn a_search_table_answers_a_match() {
    let connection = start_inillucent(AREA, "match");
    seed(&connection);
    let found = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'eligibility' ORDER BY rank",
    );
    assert!(
        found.contains(&"1".to_string()),
        "the document about eligibility is found: {found:?}"
    );
    assert!(
        !found.contains(&"4".to_string()),
        "the weather is not: {found:?}"
    );
}

/// The table-valued spelling and the `MATCH` spelling are the same query.
///
/// They have to be: an argument to a table-valued call *is* an equality on a
/// hidden column, so if the two ever disagreed one of them would be reaching
/// the module by a path the other does not.
#[test]
fn the_two_spellings_agree() {
    let connection = start_inillucent(AREA, "spellings");
    seed(&connection);
    let matched = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount' AND k = 3 ORDER BY rank",
    );
    let called = column(
        &connection,
        "SELECT rowid FROM docs('discount', 3) ORDER BY rank",
    );
    assert_eq!(matched, called);
    assert!(!matched.is_empty(), "the query found something");
}

/// `k` decides how deep the retrieval went, and it is not `LIMIT`.
#[test]
fn the_hit_count_is_the_modules_own_control() {
    let connection = start_inillucent(AREA, "depth");
    seed(&connection);
    let shallow = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 1",
    );
    let deep = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 5",
    );
    assert_eq!(shallow.len(), 1);
    assert!(deep.len() > shallow.len(), "{deep:?}");
}

/// `ORDER BY rank` is best first, and the module says so rather than sorting.
#[test]
fn rank_orders_best_first() {
    let connection = start_inillucent(AREA, "rank");
    seed(&connection);
    let ranked = rows(
        &connection,
        "SELECT rowid, rank FROM docs WHERE docs MATCH 'eligible account' ORDER BY rank",
    );
    assert!(ranked.len() >= 2, "{ranked:?}");
    let scores: Vec<f64> = ranked
        .iter()
        .filter_map(|row| row.get(1))
        .filter_map(|text| text.parse::<f64>().ok())
        .collect();
    assert_eq!(scores.len(), ranked.len(), "every row has a rank");
    for pair in scores.windows(2) {
        if let [first, second] = pair {
            assert!(first <= second, "ranks ascend: {scores:?}");
        }
    }
}

/// A rolled-back insert leaves no trace in the ranking.
///
/// This is the acceptance criterion of the phase in one test: the search index
/// is undone by the same `ROLLBACK` that undoes the row, because it *is* rows.
#[test]
fn a_rolled_back_write_is_invisible_to_the_search() {
    let connection = start_inillucent(AREA, "rollback");
    seed(&connection);
    let before = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'tirzepatide'",
    );
    assert!(before.is_empty(), "{before:?}");
    exec(&connection, "BEGIN");
    exec(
        &connection,
        "INSERT INTO docs(rowid, title, body) VALUES (99, 'Trial', 'tirzepatide dosing schedule')",
    );
    let inside = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'tirzepatide'",
    );
    assert_eq!(
        inside,
        vec!["99".to_string()],
        "a transaction sees its own writes"
    );
    exec(&connection, "ROLLBACK");
    let after = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'tirzepatide'",
    );
    assert!(
        after.is_empty(),
        "the rollback took the hit back: {after:?}"
    );
    let stored = column(&connection, "SELECT count(*) FROM docs_content");
    assert_eq!(stored, vec![CORPUS.len().to_string()]);
}

/// A committed write is found by the next query, and by the next connection.
#[test]
fn a_committed_write_survives_reopening() {
    let path = scratch(AREA, "reopen", "inillucent");
    {
        let database = Database::open(&path).expect("it opens");
        let connection = database.session();
        seed(&connection);
        exec(&connection, "BEGIN");
        exec(
            &connection,
            "INSERT INTO docs(rowid, title, body) VALUES (99, 'Trial', 'tirzepatide dosing schedule')",
        );
        exec(&connection, "COMMIT");
    }
    let database = Database::open(&path).expect("it reopens");
    let connection = database.session();
    let found = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'tirzepatide'",
    );
    assert_eq!(found, vec!["99".to_string()]);
}

/// A savepoint takes back exactly what it covered and no more.
#[test]
fn a_savepoint_takes_back_what_it_covered() {
    let connection = start_inillucent(AREA, "savepoint");
    seed(&connection);
    exec(&connection, "BEGIN");
    exec(
        &connection,
        "INSERT INTO docs(rowid, title, body) VALUES (98, 'Kept', 'semaglutide dosing schedule')",
    );
    exec(&connection, "SAVEPOINT sp1");
    exec(
        &connection,
        "INSERT INTO docs(rowid, title, body) VALUES (99, 'Dropped', 'tirzepatide dosing schedule')",
    );
    exec(&connection, "ROLLBACK TO sp1");
    exec(&connection, "COMMIT");
    assert_eq!(
        column(
            &connection,
            "SELECT rowid FROM docs WHERE docs MATCH 'semaglutide'"
        ),
        vec!["98".to_string()]
    );
    assert!(column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'tirzepatide'"
    )
    .is_empty());
}

/// A delete removes the row from the ranking, not only from the table.
#[test]
fn a_delete_removes_the_row_from_the_ranking() {
    let connection = start_inillucent(AREA, "delete");
    seed(&connection);
    exec(&connection, "DELETE FROM docs WHERE rowid = 1");
    let found = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'eligibility'",
    );
    assert!(!found.contains(&"1".to_string()), "{found:?}");
    assert_eq!(
        column(&connection, "SELECT count(*) FROM docs_content"),
        vec![(CORPUS.len() - 1).to_string()]
    );
}

/// An update re-indexes the row rather than leaving the old terms behind.
#[test]
fn an_update_reindexes_the_row() {
    let connection = start_inillucent(AREA, "update");
    seed(&connection);
    exec(
        &connection,
        "UPDATE docs SET body = 'the forecast is now sunshine' WHERE rowid = 4",
    );
    assert!(
        column(
            &connection,
            "SELECT rowid FROM docs WHERE docs MATCH 'rain'"
        )
        .is_empty(),
        "the old terms are gone"
    );
    assert_eq!(
        column(
            &connection,
            "SELECT rowid FROM docs WHERE docs MATCH 'sunshine'"
        ),
        vec!["4".to_string()]
    );
}

/// Every change one transaction made carries one commit sequence.
#[test]
fn one_transaction_publishes_one_commit_sequence() {
    let connection = start_inillucent(AREA, "sequence");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 0)",
    );
    exec(&connection, "BEGIN");
    for (id, title, body) in CORPUS {
        exec(
            &connection,
            &format!("INSERT INTO docs(rowid, title, body) VALUES ({id}, '{title}', '{body}')"),
        );
    }
    exec(&connection, "COMMIT");
    exec(&connection, "BEGIN");
    exec(
        &connection,
        "INSERT INTO docs(rowid, title, body) VALUES (6, 'Later', 'a later document')",
    );
    exec(&connection, "COMMIT");
    let sequences = column(
        &connection,
        "SELECT DISTINCT commit_seq FROM docs_delta ORDER BY commit_seq",
    );
    assert_eq!(
        sequences,
        vec!["1".to_string(), "2".to_string()],
        "two transactions, two sequences"
    );
    let first = column(
        &connection,
        "SELECT count(*) FROM docs_delta WHERE commit_seq = 1",
    );
    assert_eq!(first, vec![CORPUS.len().to_string()]);
}

/// Compaction folds the log into a new generation and changes no answer.
#[test]
fn compaction_publishes_a_generation_and_changes_no_answer() {
    let connection = start_inillucent(AREA, "compact");
    seed(&connection);
    let before = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 5 ORDER BY rank",
    );
    assert_eq!(
        column(
            &connection,
            "SELECT v FROM docs_state WHERE k = 'generation'"
        ),
        vec!["0".to_string()],
        "nothing has been compacted yet"
    );
    exec(&connection, "INSERT INTO docs(docs) VALUES ('compact')");
    assert_eq!(
        column(
            &connection,
            "SELECT v FROM docs_state WHERE k = 'generation'"
        ),
        vec!["1".to_string()]
    );
    assert_eq!(
        column(&connection, "SELECT count(*) FROM docs_delta"),
        vec!["0".to_string()],
        "the log was folded in"
    );
    let after = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 5 ORDER BY rank",
    );
    assert_eq!(before, after, "compaction changed no answer");
}

/// A generation is never removed by a write, only by asking.
#[test]
fn an_old_generation_survives_until_it_is_dropped() {
    let connection = start_inillucent(AREA, "generations");
    seed(&connection);
    exec(&connection, "INSERT INTO docs(docs) VALUES ('compact')");
    exec(
        &connection,
        "INSERT INTO docs(rowid, title, body) VALUES (6, 'More', 'another eligible account')",
    );
    exec(&connection, "INSERT INTO docs(docs) VALUES ('compact')");
    let generations = column(
        &connection,
        "SELECT DISTINCT generation FROM docs_gen ORDER BY generation",
    );
    assert_eq!(
        generations,
        vec!["1".to_string(), "2".to_string()],
        "the superseded generation is still reachable"
    );
    exec(
        &connection,
        "INSERT INTO docs(docs) VALUES ('drop-old-generations')",
    );
    assert_eq!(
        column(
            &connection,
            "SELECT DISTINCT generation FROM docs_gen ORDER BY generation"
        ),
        vec!["2".to_string()]
    );
}

/// A rebuild reproduces the index from the rows alone.
#[test]
fn a_rebuild_reproduces_the_index_from_the_rows() {
    let connection = start_inillucent(AREA, "rebuild");
    seed(&connection);
    let before = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 5 ORDER BY rank",
    );
    exec(&connection, "INSERT INTO docs(docs) VALUES ('rebuild')");
    let after = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'discount eligible' AND k = 5 ORDER BY rank",
    );
    assert_eq!(before, after);
}

/// A vector table finds the row nearest a query vector, exactly.
#[test]
fn a_vector_table_finds_the_nearest_row() {
    let connection = start_inillucent(AREA, "vector");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE points USING inillucent_search(label, dims = 4)",
    );
    // Unit vectors along each axis, so the nearest neighbour of an axis is
    // itself and the answer is not a matter of opinion.
    let axes: [(i64, &str, [f32; 4]); 4] = [
        (1, "x", [1.0, 0.0, 0.0, 0.0]),
        (2, "y", [0.0, 1.0, 0.0, 0.0]),
        (3, "z", [0.0, 0.0, 1.0, 0.0]),
        (4, "w", [0.0, 0.0, 0.0, 1.0]),
    ];
    for (id, label, vector) in axes {
        exec(
            &connection,
            &format!(
                "INSERT INTO points(rowid, label, vector) VALUES ({id}, '{label}', x'{}')",
                hex(&vector)
            ),
        );
    }
    let found = column(
        &connection,
        &format!(
            "SELECT label FROM points WHERE vector = x'{}' AND k = 1",
            hex(&[0.0, 0.0, 1.0, 0.0])
        ),
    );
    assert_eq!(found, vec!["z".to_string()]);
}

/// A vector of the wrong width is refused rather than resized.
#[test]
fn a_vector_of_the_wrong_width_is_refused() {
    let connection = start_inillucent(AREA, "vector-width");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE points USING inillucent_search(label, dims = 4)",
    );
    let refused = try_exec(
        &connection,
        &format!(
            "INSERT INTO points(rowid, label, vector) VALUES (1, 'x', x'{}')",
            hex(&[1.0, 0.0])
        ),
    );
    assert!(refused.is_err(), "{refused:?}");
}

/// A lexical-only table refuses a vector rather than ignoring it.
#[test]
fn a_lexical_table_refuses_a_vector() {
    let connection = start_inillucent(AREA, "no-vectors");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(body)",
    );
    let refused = try_exec(
        &connection,
        &format!(
            "INSERT INTO docs(rowid, body, vector) VALUES (1, 'text', x'{}')",
            hex(&[1.0])
        ),
    );
    assert!(refused.is_err(), "{refused:?}");
}

/// The integrity check reports a shadow table that has been edited underneath
/// the module.
///
/// Shadow tables are ordinary tables, so anything that can write the database
/// can write one. The module cannot stop that; what it can do is notice.
#[test]
fn the_integrity_check_notices_an_edited_shadow_table() {
    let connection = start_inillucent(AREA, "integrity");
    seed(&connection);
    assert!(
        try_exec(
            &connection,
            "INSERT INTO docs(docs) VALUES ('integrity-check')"
        )
        .is_ok(),
        "a healthy index passes"
    );
    exec(&connection, "DELETE FROM docs_content WHERE id = 3");
    let complained = try_exec(
        &connection,
        "INSERT INTO docs(docs) VALUES ('integrity-check')",
    );
    assert!(complained.is_err(), "the check noticed: {complained:?}");
}

/// A command nobody implemented is an error, not a silent no-op.
#[test]
fn an_unknown_command_is_refused() {
    let connection = start_inillucent(AREA, "command");
    seed(&connection);
    assert!(try_exec(&connection, "INSERT INTO docs(docs) VALUES ('reticulate')").is_err());
}

/// The declaration a table was created with is what it reports.
#[test]
fn the_declaration_is_stored_and_read_back() {
    let connection = start_inillucent(AREA, "declaration");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(body, dims = 8, mode = 'approximate')",
    );
    let stored = rows(&connection, "SELECT k, v FROM docs_config ORDER BY k");
    let pairs: Vec<(String, String)> = stored
        .into_iter()
        .filter_map(|row| match (row.first(), row.get(1)) {
            (Some(key), Some(value)) => Some((key.clone(), value.clone())),
            _ => None,
        })
        .collect();
    assert!(
        pairs.contains(&("dims".to_string(), "8".to_string())),
        "{pairs:?}"
    );
    assert!(pairs.contains(&("mode".to_string(), "approximate".to_string())));
    assert!(pairs.contains(&("metric".to_string(), "cosine".to_string())));
    assert!(pairs.contains(&("tokenize".to_string(), "porter".to_string())));
}

/// A distance this build cannot compute is refused when the table is made.
#[test]
fn an_unimplemented_metric_is_refused_at_create() {
    let connection = start_inillucent(AREA, "metric");
    assert!(try_exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(body, metric = 'euclidean')"
    )
    .is_err());
}

/// A search table joins like a table, because it is one.
#[test]
fn a_search_table_joins() {
    let connection = start_inillucent(AREA, "join");
    seed(&connection);
    exec(
        &connection,
        "CREATE TABLE owner(id INTEGER PRIMARY KEY, who TEXT)",
    );
    exec(
        &connection,
        "INSERT INTO owner VALUES (1, 'ada'), (2, 'grace')",
    );
    let joined = rows(
        &connection,
        "SELECT owner.who FROM docs JOIN owner ON owner.id = docs.rowid \
         WHERE docs MATCH 'discount' AND k = 5 ORDER BY owner.who",
    );
    let names: Vec<String> = joined
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .collect();
    assert!(
        names.contains(&"ada".to_string()) || names.contains(&"grace".to_string()),
        "{names:?}"
    );
}

/// The auxiliary functions answer about the row the cursor is on.
#[test]
fn the_auxiliary_functions_describe_the_hit() {
    let connection = start_inillucent(AREA, "auxiliary");
    seed(&connection);
    let described = rows(
        &connection,
        "SELECT score(docs), confidence(docs), origin(docs) FROM docs \
         WHERE docs MATCH 'eligibility' AND k = 1",
    );
    let first = described.first().expect("one hit");
    assert_ne!(first.first().map(String::as_str), Some("NULL"));
    assert_eq!(first.get(2).map(String::as_str), Some("lexical"));
}

/// Returns a little-endian `f32` blob as hexadecimal, for an `x'...'` literal.
fn hex(vector: &[f32]) -> String {
    let mut out = String::new();
    for value in vector {
        for byte in value.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

// --- M8: what an ordinary write pays for the graph ---
//
// The finding these guard is that an incremental vector insert rebuilt the
// whole HNSW graph. It did: automatic compaction ran `build_from_rows`, which
// reads every row and inserts every chunk into a fresh graph, and it ran inside
// the committing transaction - so one `INSERT` into a 598,560 chunk table paid
// a nine and a half minute build.
//
// The bound is stated as a count rather than as a clock. `%_state`'s `inserted`
// row says how many chunks the last generation build put into the graph, so a
// fold that started reading the corpus again fails these on any machine under
// any load, which a stopwatch cannot promise.

/// Returns the integer a `%_state` row holds.
///
/// @param connection - the database
/// @param table - the search table's name
/// @param key - the state key to read
fn state(connection: &Connection<'_>, table: &str, key: &str) -> i64 {
    column(
        connection,
        &format!("SELECT v FROM {table}_state WHERE k = '{key}'"),
    )
    .first()
    .map(|text| text.parse::<i64>().unwrap_or(-1))
    .unwrap_or(-1)
}

/// Opens a database that is already there, without emptying it first.
///
/// **Leaked rather than borrowed**, for the same reason
/// `differential::start_inillucent` leaks: the new engine's `Connection<'d>`
/// borrows the `Database` it came from, and this helper's whole job is to hand
/// a connection back to a caller that never sees the database, the way the old
/// engine's owned `Connection` did.
///
/// @param path - the file to open
fn open_at(path: &std::path::Path) -> Connection<'static> {
    let database: &'static Database =
        Box::leak(Box::new(Database::open(path).expect("the database opens")));
    let connection = database.session();
    let _ = connection.execute_batch("PRAGMA busy_timeout = 5000");
    connection
}

/// Writes `count` rows, each in its own transaction, from `first`.
///
/// One row per commit on purpose: the fold runs at commit, so this is the shape
/// that puts the most folds into a run, and it is the shape an application
/// syncing a mailbox actually has.
///
/// @param connection - the database
/// @param first - the first rowid to write
/// @param count - how many rows to write
fn write_rows(connection: &Connection<'_>, first: i64, count: i64) {
    for offset in 0..count {
        let id = first + offset;
        exec(
            connection,
            &format!(
                "INSERT INTO docs(rowid, title, body) VALUES \
                 ({id}, 'title {id}', 'the discount applies to eligible accounts number {id}')"
            ),
        );
    }
}

/// A fold inserts the rows the commit wrote, not the rows the table holds.
#[test]
fn a_fold_inserts_what_the_commit_wrote_and_not_the_corpus() {
    let connection = start_inillucent(AREA, "fold-bounded");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 8)",
    );

    // The first fold has no generation to fold into, so it builds the eight
    // rows the table holds - which is still only what has been written.
    write_rows(&connection, 1, 8);
    assert_eq!(state(&connection, "docs", "generation"), 1);
    assert_eq!(state(&connection, "docs", "inserted"), 8);
    assert_eq!(state(&connection, "docs", "folds"), 1);

    // Eight times the corpus later, a fold is still eight inserts. This is the
    // whole finding: before M8 the second number was 64.
    write_rows(&connection, 9, 56);
    assert_eq!(state(&connection, "docs", "rows"), 64);
    assert_eq!(
        state(&connection, "docs", "inserted"),
        8,
        "a fold's graph work must not grow with the corpus it folds into"
    );
    assert_eq!(state(&connection, "docs", "folds"), 8);
    assert_eq!(state(&connection, "docs", "generation"), 8);
}

/// The `compact` command is the one that reads every row.
#[test]
fn the_compact_command_builds_from_every_row() {
    let connection = start_inillucent(AREA, "fold-compact");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 8)",
    );
    write_rows(&connection, 1, 64);
    assert_eq!(state(&connection, "docs", "inserted"), 8);

    exec(&connection, "INSERT INTO docs(docs) VALUES ('compact')");
    assert_eq!(
        state(&connection, "docs", "inserted"),
        64,
        "an explicit compaction is the batch rebuild, and it costs the corpus"
    );
    assert_eq!(
        state(&connection, "docs", "folds"),
        0,
        "a single-pass build starts the lineage again"
    );
}

/// An empty delta log does not stop a caller asking for a clean graph.
///
/// It used to. That was invisible while automatic compaction also built in one
/// pass; now that a commit folds, the table with an empty log is precisely the
/// one whose graph has the most tombstoned chunks in it.
#[test]
fn compaction_is_available_after_the_log_has_been_folded_away() {
    let connection = start_inillucent(AREA, "fold-empty-log");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 8)",
    );
    write_rows(&connection, 1, 16);
    assert_eq!(
        column(&connection, "SELECT count(*) FROM docs_delta"),
        vec!["0".to_string()],
        "the log was folded away"
    );
    let before = state(&connection, "docs", "generation");
    exec(&connection, "INSERT INTO docs(docs) VALUES ('compact')");
    assert_eq!(state(&connection, "docs", "generation"), before + 1);
    assert_eq!(state(&connection, "docs", "inserted"), 16);
}

/// Folding changes no answer, and neither does the rebuild after it.
#[test]
fn folding_and_rebuilding_answer_the_same_query_the_same_way() {
    const QUERY: &str = "SELECT rowid FROM docs WHERE docs MATCH 'eligible accounts' \
                         AND k = 10 ORDER BY rank";

    let unfolded = {
        let connection = start_inillucent(AREA, "fold-answers-plain");
        exec(
            &connection,
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 0)",
        );
        write_rows(&connection, 1, 40);
        column(&connection, QUERY)
    };

    let folded = start_inillucent(AREA, "fold-answers-folded");
    exec(
        &folded,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 8)",
    );
    write_rows(&folded, 1, 40);
    assert!(state(&folded, "docs", "folds") >= 4, "it folded");
    assert_eq!(
        column(&folded, QUERY),
        unfolded,
        "folding changed the ranking"
    );

    exec(&folded, "INSERT INTO docs(docs) VALUES ('compact')");
    assert_eq!(
        column(&folded, QUERY),
        unfolded,
        "the rebuild after the folds changed the ranking"
    );
}

/// An update folded in leaves the chunk it replaced behind until a rebuild.
///
/// This is the price of not rebuilding, and it is stated as a number an
/// application can read: `chunks` counts what the graph holds and `rows` counts
/// what the table holds, so the difference is the dead weight folding left.
#[test]
fn an_update_leaves_a_dead_chunk_until_the_rebuild_removes_it() {
    let connection = start_inillucent(AREA, "fold-tombstones");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 8)",
    );
    write_rows(&connection, 1, 16);
    assert_eq!(state(&connection, "docs", "chunks"), 16);

    for id in 1..=8 {
        exec(
            &connection,
            &format!("UPDATE docs SET body = 'a revised eligible account {id}' WHERE rowid = {id}"),
        );
    }
    assert_eq!(state(&connection, "docs", "rows"), 16);
    assert_eq!(
        state(&connection, "docs", "chunks"),
        24,
        "the eight replaced chunks are still in the graph"
    );

    exec(&connection, "INSERT INTO docs(docs) VALUES ('compact')");
    assert_eq!(
        state(&connection, "docs", "chunks"),
        16,
        "the rebuild is what removes them"
    );
}

/// A folded index survives being closed and opened again.
///
/// **The first connection is a plain, stack-owned `Database` rather than
/// `open_at`'s leaked one.** This engine holds a writer's main-file lock for
/// the connection's whole life - see `pragma.rs`'s `data_version` comment -
/// and `open_at` leaks its `Database` on purpose so a `Connection<'static>`
/// can outlive the function that built it, which is exactly wrong here: a
/// leaked `Database` is never dropped, so its lock is never released, and a
/// second, independent open of the same path is refused with `BUSY` no
/// matter how cleanly the first connection finished its writes. Scoping an
/// owned `Database` to this block, so it drops - and releases the lock -
/// before `reopened` is opened, is what makes "closed and opened again" the
/// question this test actually asks.
#[test]
fn a_folded_index_reopens_and_answers() {
    const QUERY: &str = "SELECT rowid FROM docs WHERE docs MATCH 'eligible accounts' \
                         AND k = 10 ORDER BY rank";
    let path = scratch(AREA, "fold-reopen", "inillucent");
    let expected = {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        let _ = connection.execute_batch("PRAGMA busy_timeout = 5000");
        exec(
            &connection,
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, compact = 8)",
        );
        write_rows(&connection, 1, 40);
        column(&connection, QUERY)
        // `connection`, then `database`, drop here - closing the first
        // connection for real, rather than leaking it.
    };
    let reopened = open_at(&path);
    assert_eq!(column(&reopened, QUERY), expected);
    assert_eq!(state(&reopened, "docs", "folds"), 5);
}

/// Creates a faceted search table and fills it.
///
/// Every row holds both query terms, at a distance that varies with the rowid,
/// so the position rescore has something to move and BM25 has something to
/// order. Every eleventh row is marked not live, which is the rate the
/// migration's own corpus suite tombstones at.
/// @param connection - the database
/// @param rows - how many rows to write
fn seed_faceted(connection: &Connection<'_>, rows: usize) {
    exec(
        connection,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(body, live FACET)",
    );
    for id in 0..rows {
        let filler: String = (0..(id % 17)).map(|n| format!("filler{n} ")).collect();
        let tail: String = (0..(id % 7)).map(|n| format!("tail{n} ")).collect();
        let body = format!("alpha {filler}beta {tail}gamma delta epsilon body{id}");
        let live = if id % 11 == 10 { "0" } else { "1" };
        exec(
            connection,
            &format!("INSERT INTO docs(rowid, body, live) VALUES ({id}, '{body}', '{live}')"),
        );
    }
}

/// A facet constrains which rows a search ranks over.
#[test]
fn a_facet_constrains_the_search() {
    let connection = start_inillucent(AREA, "facet-constrains");
    seed_faceted(&connection, 60);
    let live = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'alpha beta' AND k = 20 AND live = '1' \
         ORDER BY rank",
    );
    assert_eq!(live.len(), 20, "the search fills the k it was asked for");
    for id in &live {
        let ordinal: usize = id.parse().expect("a rowid");
        assert!(
            ordinal % 11 != 10,
            "a row marked not live answered: {live:?}"
        );
    }
    let dead = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'alpha beta' AND k = 20 AND live = '0' \
         ORDER BY rank",
    );
    assert!(!dead.is_empty(), "the other value selects the other rows");
    for id in &dead {
        let ordinal: usize = id.parse().expect("a rowid");
        assert_eq!(ordinal % 11, 10, "a live row answered: {dead:?}");
    }
}

/// A facet is applied inside the search, not to what the search answered.
///
/// **This is the property the feature exists for** (task-2067).
/// `Bm25Index::top_k` rescores the best `k * rescore_depth_factor` hits by
/// where the query's terms sit inside them, the rescore only ever lowers a
/// score, and a hit outside that window keeps its full score and competes
/// against rescored ones. Which hits are inside the window depends on which
/// rows the scan admitted, so a constraint the ranking saw and the same
/// constraint applied afterwards are different answers.
///
/// The test says so by comparing the two: a search with the facet constrained,
/// against an unconstrained search with the same rows removed from its answer.
/// If those two agreed there would be nothing here worth building, and a later
/// change that quietly moved the facet out of the scan would make them agree.
#[test]
fn a_facet_is_applied_inside_the_search_rather_than_to_its_answer() {
    let connection = start_inillucent(AREA, "facet-inside");
    seed_faceted(&connection, 400);
    let inside = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'gamma delta epsilon alpha' AND k = 10 \
         AND live = '1' ORDER BY rank",
    );
    let afterwards: Vec<String> = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'gamma delta epsilon alpha' AND k = 10 \
         ORDER BY rank",
    )
    .into_iter()
    .filter(|id| id.parse::<usize>().is_ok_and(|ordinal| ordinal % 11 != 10))
    .collect();
    assert_eq!(inside.len(), 10, "the constrained search fills its k");
    assert_ne!(
        inside, afterwards,
        "if these agree the facet is no longer reaching the scan"
    );
}

/// A facet's value is not part of the text a query matches.
///
/// Both halves matter. Matching on the value finds nothing, because the value
/// was never tokenised; and the ranking of the prose beside it is the ranking
/// an unfaceted table gives, because a value in the indexed text would change
/// the terms, the row's length and therefore its score.
#[test]
fn a_facet_value_is_not_indexed_as_text() {
    let connection = start_inillucent(AREA, "facet-not-text");
    seed_faceted(&connection, 60);
    let matched = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'gamma1' AND k = 10 ORDER BY rank",
    );
    assert!(
        matched.is_empty(),
        "the facet's value is not a term: {matched:?}"
    );

    let plain = start_inillucent(AREA, "facet-not-text-plain");
    exec(
        &plain,
        "CREATE VIRTUAL TABLE docs USING inillucent_search(body)",
    );
    for id in 0..60usize {
        let filler: String = (0..(id % 17)).map(|n| format!("filler{n} ")).collect();
        let tail: String = (0..(id % 7)).map(|n| format!("tail{n} ")).collect();
        let body = format!("alpha {filler}beta {tail}gamma delta epsilon body{id}");
        exec(
            &plain,
            &format!("INSERT INTO docs(rowid, body) VALUES ({id}, '{body}')"),
        );
    }
    const QUERY: &str = "SELECT rowid FROM docs WHERE docs MATCH 'alpha beta gamma' AND k = 10 \
                         ORDER BY rank";
    assert_eq!(
        column(&connection, QUERY),
        column(&plain, QUERY),
        "a facet column changes no ranking of the text beside it"
    );
}

/// A facet is a column like any other: it comes back from a select.
#[test]
fn a_facet_is_still_a_column() {
    let connection = start_inillucent(AREA, "facet-column");
    seed_faceted(&connection, 20);
    assert_eq!(
        column(&connection, "SELECT live FROM docs WHERE rowid = 10"),
        vec!["0".to_string()]
    );
    assert_eq!(
        column(&connection, "SELECT live FROM docs WHERE rowid = 9"),
        vec!["1".to_string()]
    );
}

/// A facet constraint on a plain scan is still the engine's to evaluate.
///
/// The module claims a facet constraint only when it is ranking, because
/// claiming one tells the engine not to evaluate it - and on a scan there is no
/// ranking to push it into, so the engine is the only thing that would.
#[test]
fn a_facet_on_a_scan_still_selects() {
    let connection = start_inillucent(AREA, "facet-scan");
    seed_faceted(&connection, 33);
    let found = column(&connection, "SELECT rowid FROM docs WHERE live = '0'");
    assert_eq!(
        found,
        vec!["10".to_string(), "21".to_string(), "32".to_string()],
        "every row marked not live, and nothing else"
    );
}

/// A faceted table survives being closed and opened again.
///
/// The declaration lives in `%_config`, and a reopen reads it from there rather
/// than from the `CREATE` text - so this is the check that the stored rows say
/// which column is a facet, not only the statement that made it.
#[test]
fn a_faceted_table_reopens_and_still_filters() {
    const QUERY: &str = "SELECT rowid FROM docs WHERE docs MATCH 'alpha beta' AND k = 10 \
                         AND live = '0' ORDER BY rank";
    let path = scratch(AREA, "facet-reopen", "inillucent");
    let expected = {
        let database = Database::open(&path).expect("the database opens");
        let connection = database.session();
        let _ = connection.execute_batch("PRAGMA busy_timeout = 5000");
        seed_faceted(&connection, 60);
        column(&connection, QUERY)
    };
    assert!(!expected.is_empty(), "the query finds something to compare");
    let reopened = open_at(&path);
    assert_eq!(column(&reopened, QUERY), expected);
}

/// A faceted table stores the later format, and a plain one does not.
///
/// The number is what carries the refusal, so this asserts the number. An
/// older build compares the stored number against the one it knows and refuses,
/// which is the whole of the protection - and it is protection worth having,
/// because such a build would otherwise index the facet's value as prose and
/// answer a ranking the table was not written to answer. Keeping a plain table
/// on the first format is the other half: nothing that declares no facet
/// becomes unreadable to a build already installed.
#[test]
fn only_a_faceted_table_stores_the_later_format() {
    let connection = start_inillucent(AREA, "facet-format");
    seed_faceted(&connection, 5);
    exec(
        &connection,
        "CREATE VIRTUAL TABLE plain USING inillucent_search(body)",
    );
    assert_eq!(
        column(&connection, "SELECT v FROM docs_config WHERE k = 'format'"),
        vec!["2".to_string()]
    );
    assert_eq!(
        column(&connection, "SELECT v FROM plain_config WHERE k = 'format'"),
        vec!["1".to_string()]
    );
    assert_eq!(
        column(&connection, "SELECT v FROM docs_config WHERE k = 'facets'"),
        vec!["live".to_string()]
    );
}

/// A declaration of nothing but facets is refused.
#[test]
fn a_table_of_facets_alone_is_refused() {
    let connection = start_inillucent(AREA, "facet-alone");
    // Read straight off the error rather than through `try_exec`, which keeps
    // only the message: the sentence that names the reason is the detail, and
    // the message of a refusal like this one is "SQL logic error".
    let failed = connection
        .execute_batch("CREATE VIRTUAL TABLE docs USING inillucent_search(live FACET)")
        .expect_err("a table with no text column is refused");
    let said = failed
        .detail()
        .unwrap_or_else(|| failed.message())
        .to_string();
    assert!(said.contains("not a facet"), "the refusal says why: {said}");
}
