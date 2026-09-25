//! What the incremental write path actually retrieves.
//!
//! Invariant: **a corpus written through the virtual table, across flushes,
//! merges and a compaction, answers what the same corpus answers when it is
//! built in one pass.** The single-pass build is the reference because it is
//! the simplest thing the retrieval engine does: read every row, build one
//! graph, build one lexical index. Everything else - the delta log, the publish
//! rule, the segment merge cascade, the tombstones - exists to approximate it
//! incrementally, and an approximation nobody measures is a guess.
//!
//! ## The gap this closes (task-1932, M4)
//!
//! The scorecard never drove `inillucent-search` at all.
//! `crates/inillucent-bench/src/engine.rs` imports `inillucent_core::index::Index`
//! and builds in one pass; `crates/inillucent-compat/src/bin/baseline.rs` pins
//! `inillucent-core` and `inillucent-bench`. So every number the project
//! published about retrieval was a number about the one path that was not
//! changing, and the four commits before this ticket changed the delta log, the
//! publish rule, the merge cascade and the tombstones - none of which anything
//! measured the recall of.
//!
//! ## What is measured, and what the tolerance means
//!
//! Recall@10 of the incremental path against the single-pass build, over the
//! same rows and the same queries. [`TOLERANCE`] is how much lower the
//! incremental path is allowed to be, and it is not zero for a real reason: an
//! HNSW graph grown one insert at a time is not the graph a single pass
//! produces, which is the whole reason `compact` exists. It is not large
//! either - a gap past it means the incremental path is losing documents rather
//! than ordering them slightly differently.
//!
//! The lexical side is asserted exactly, because BM25 over the same rows has no
//! such freedom: a term that matches a document matches it whatever order the
//! rows arrived in.

use std::collections::BTreeSet;

use inillucent_compat::differential::start_inillucent;
use inillucent_engine::connect::Connection;

/// Where this suite's scratch databases live.
const AREA: &str = "search-recall";

/// How many documents the corpus holds.
///
/// A few thousand, which is enough that the graph has more than one layer and
/// that a merge cascade actually cascades - and small enough that the whole
/// case runs in seconds.
const DOCUMENTS: usize = 3_000;

/// How many documents each write transaction carries.
///
/// Small enough that the corpus takes many transactions, which is what produces
/// the flushes, the segments and the merges this suite is about. A corpus
/// written in one transaction exercises none of them.
const BATCH: usize = 120;

/// How much lower the incremental path's recall may be than the single pass.
///
/// **Declared rather than derived, and deliberately not zero.** A graph grown
/// one insert at a time is not the graph a single pass produces; `compact`
/// exists because of that. What this number says is how much of a difference is
/// the known one and how much would be documents going missing.
const TOLERANCE: f64 = 0.05;

/// How many results each query asks for.
const AT: usize = 10;

/// Runs a statement for its effect.
fn exec(connection: &Connection<'_>, sql: &str) {
    connection
        .execute_batch(sql)
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
}

/// Returns the first column of every row, as text.
fn column(connection: &Connection<'_>, sql: &str) -> Vec<String> {
    let mut statement = connection
        .prepare(sql)
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
    let mut out = Vec::new();
    while statement
        .step()
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()))
    {
        let Some(first) = statement.row().first() else {
            continue;
        };
        out.push(match first {
            inillucent_tree::datum::OwnedDatum::Text(bytes) => {
                String::from_utf8_lossy(bytes).into_owned()
            }
            inillucent_tree::datum::OwnedDatum::Int(value) => value.to_string(),
            other => format!("{other:?}"),
        });
    }
    out
}

/// The vocabulary the corpus is built from.
///
/// Deliberately small and overlapping, so that a query matches many documents
/// and recall has room to differ. A corpus of unique words would make every
/// query match one row, and every path would score 1.0.
const WORDS: [&str; 24] = [
    "eligibility",
    "member",
    "plan",
    "coverage",
    "claim",
    "submission",
    "appeal",
    "window",
    "denial",
    "provider",
    "network",
    "referral",
    "copay",
    "deductible",
    "formulary",
    "prior",
    "authorization",
    "benefit",
    "premium",
    "enrollment",
    "dependent",
    "renewal",
    "grievance",
    "adjudication",
];

/// Returns one document's body, built from the vocabulary by a fixed rule.
///
/// A fixed rule rather than a random one: a recall number that moves between
/// runs is a number nobody can act on, and the point of this suite is a number
/// two paths can be compared on.
///
/// @param nth - the document's number
fn body(nth: usize) -> String {
    let mut words = Vec::new();
    for step in 0..7usize {
        let at = nth
            .saturating_mul(step.saturating_add(3))
            .saturating_add(step)
            % WORDS.len();
        words.push(WORDS.get(at).copied().unwrap_or("member"));
    }
    words.join(" ")
}

/// The queries every arm is scored on.
///
/// Every word in the vocabulary, so the score is over the whole corpus rather
/// than over whichever queries happened to be picked.
fn queries() -> Vec<&'static str> {
    WORDS.to_vec()
}

/// Returns the document ids one query answers, in rank order.
///
/// @param connection - the database to ask
/// @param table - which table to query
/// @param term - the query
fn answered(connection: &Connection<'_>, table: &str, term: &str) -> Vec<String> {
    column(
        connection,
        &format!("SELECT rowid FROM {table} WHERE {table} MATCH '{term}' ORDER BY rank LIMIT {AT}"),
    )
}

/// Fills a table with the corpus, a batch per transaction.
///
/// @param connection - the database to write to
/// @param table - the table to fill
/// @param maintain - whether to run the maintenance commands between batches
fn fill(connection: &Connection<'_>, table: &str, maintain: bool) {
    let mut written = 0usize;
    while written < DOCUMENTS {
        exec(connection, "BEGIN");
        let end = written.saturating_add(BATCH).min(DOCUMENTS);
        for nth in written..end {
            exec(
                connection,
                &format!(
                    "INSERT INTO {table}(rowid, body) VALUES ({}, '{}')",
                    nth.saturating_add(1),
                    body(nth)
                ),
            );
        }
        exec(connection, "COMMIT");
        written = end;
        // **The cycles this suite exists for.** A commit flushes into a new
        // segment; enough segments cascade into a merge; a compaction folds
        // every live segment back into one. Running them between batches is
        // what makes the corpus arrive the way a real one does, rather than as
        // one write nothing has to reconcile.
        if maintain && written.is_multiple_of(BATCH.saturating_mul(5)) {
            exec(
                connection,
                &format!("INSERT INTO {table}({table}) VALUES ('compact')"),
            );
        }
    }
}

/// The incremental path retrieves what a single-pass build retrieves.
///
/// **The measurement nothing took (task-1932, M4).** Both arms hold the same
/// rows and are asked the same questions; the difference is only how the rows
/// arrived - one through many transactions with flushes, merges and compactions
/// between them, the other rebuilt in one pass at the end.
#[test]
fn the_incremental_path_retrieves_what_a_single_pass_build_retrieves() {
    let connection = start_inillucent(AREA, "incremental");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE incremental USING inillucent_search(body)",
    );
    fill(&connection, "incremental", true);

    // The reference: the same rows, rebuilt in one pass. `rebuild` is the
    // single-pass build - it reads every live row and builds one graph and one
    // lexical index, which is what `Index::build` does and what the benchmark
    // arm measures.
    let reference = start_inillucent(AREA, "single-pass");
    exec(
        &reference,
        "CREATE VIRTUAL TABLE single_pass USING inillucent_search(body)",
    );
    fill(&reference, "single_pass", false);
    exec(
        &reference,
        "INSERT INTO single_pass(single_pass) VALUES ('rebuild')",
    );

    // Both arms hold the corpus, which is the first thing to check: a recall
    // number over a table that lost its rows is a number about nothing.
    for (connection, table) in [(&connection, "incremental"), (&reference, "single_pass")] {
        let counted = column(connection, &format!("SELECT count(*) FROM {table}"));
        assert_eq!(
            counted,
            vec![DOCUMENTS.to_string()],
            "{table} holds {counted:?} rows and the corpus is {DOCUMENTS}"
        );
    }

    let mut worst = 1.0f64;
    let mut worst_query = String::new();
    let mut total = 0.0f64;
    let mut asked = 0usize;
    let mut missing_entirely = Vec::new();

    for term in queries() {
        let wanted: BTreeSet<String> = answered(&reference, "single_pass", term)
            .into_iter()
            .collect();
        if wanted.is_empty() {
            // A term nothing matches scores nothing; the vocabulary is built so
            // that most match, and one that does not is not evidence either way.
            continue;
        }
        let got: BTreeSet<String> = answered(&connection, "incremental", term)
            .into_iter()
            .collect();
        if got.is_empty() {
            missing_entirely.push(term.to_string());
            continue;
        }
        let overlap = wanted.intersection(&got).count();
        let recall = overlap as f64 / wanted.len() as f64;
        total += recall;
        asked = asked.saturating_add(1);
        if recall < worst {
            worst = recall;
            worst_query = term.to_string();
        }
    }

    assert!(
        missing_entirely.is_empty(),
        "the incremental path answered nothing at all for {missing_entirely:?}, which the \
         single-pass build answers - those documents are not being retrieved rather than \
         being ranked differently"
    );
    assert!(
        asked >= WORDS.len() / 2,
        "only {asked} of {} queries matched anything in the reference, so this run measured \
         almost nothing",
        WORDS.len()
    );

    let mean = total / asked as f64;
    assert!(
        mean >= 1.0 - TOLERANCE,
        "recall@{AT} of the incremental path is {mean:.3} against the single-pass build, and \
         the tolerance is {:.3}. The worst query was '{worst_query}' at {worst:.3}.",
        1.0 - TOLERANCE
    );
}

/// A term that matches a document matches it whichever path wrote the row.
///
/// **Asserted exactly, where the recall above has a tolerance.** BM25 over the
/// same rows has no freedom to differ: the incremental path may order two
/// equally-scored documents differently, and it may not lose one. This is the
/// half of the measurement that catches a document a merge dropped.
#[test]
fn every_document_a_term_matches_is_still_matched_after_the_merges() {
    let connection = start_inillucent(AREA, "lexical");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE lexical USING inillucent_search(body)",
    );
    fill(&connection, "lexical", true);

    // What the corpus says, computed here rather than asked of the engine: the
    // document bodies are built by a fixed rule, so which documents hold a term
    // is arithmetic rather than a second query.
    for term in queries() {
        let expected: BTreeSet<String> = (0..DOCUMENTS)
            .filter(|nth| body(*nth).split(' ').any(|word| word == term))
            .map(|nth| nth.saturating_add(1).to_string())
            .collect();
        if expected.is_empty() {
            continue;
        }
        // `k` is the module's hidden result count, and its default is ten -
        // which is the right default for a search and the wrong one for a
        // question about which documents match at all. Asked for more than the
        // corpus holds, so the answer is every match rather than the top few.
        let found: BTreeSet<String> = column(
            &connection,
            &format!(
                "SELECT rowid FROM lexical WHERE lexical MATCH '{term}' AND k = {}",
                DOCUMENTS.saturating_mul(2)
            ),
        )
        .into_iter()
        .collect();
        let lost: Vec<&String> = expected.difference(&found).collect();
        assert!(
            lost.is_empty(),
            "'{term}' matches {} documents in the corpus and the table answered {}; {} are \
             missing, first few: {:?}",
            expected.len(),
            found.len(),
            lost.len(),
            lost.iter().take(5).collect::<Vec<&&String>>()
        );
    }
}

/// A compaction does not change what the table answers.
///
/// The narrowest statement of the same property, and the cheapest to read when
/// one of the two above fails: if this one fails too, the merge cascade is
/// losing rows; if only this one passes, the loss is in the delta log or the
/// publish rule instead.
#[test]
fn a_compaction_does_not_change_what_the_table_answers() {
    let connection = start_inillucent(AREA, "compaction");
    exec(
        &connection,
        "CREATE VIRTUAL TABLE staged USING inillucent_search(body)",
    );
    fill(&connection, "staged", false);

    let before: Vec<Vec<String>> = queries()
        .into_iter()
        .map(|term| answered(&connection, "staged", term))
        .collect();
    exec(&connection, "INSERT INTO staged(staged) VALUES ('compact')");
    let after: Vec<Vec<String>> = queries()
        .into_iter()
        .map(|term| answered(&connection, "staged", term))
        .collect();

    for (term, (was, now)) in queries().into_iter().zip(before.iter().zip(after.iter())) {
        let was_set: BTreeSet<&String> = was.iter().collect();
        let now_set: BTreeSet<&String> = now.iter().collect();
        assert_eq!(
            was_set, now_set,
            "'{term}' answered {was:?} before the compaction and {now:?} after it"
        );
    }
}
