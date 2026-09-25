//! FTS5 query parity: fifty queries over a five hundred document corpus,
//! against the pinned SQLite 3.53.4.
//!
//! Invariant: **every query in `corpora/fts5-parity/queries.list` returns
//! SQLite's rows, in SQLite's order, or fails where SQLite fails.** There is no
//! allow list here. The forty one queries came out of the task-1979 review,
//! where thirteen of them disagreed; the nine after them are the cross column
//! cases the review's own corpus could not have caught, because `bm25()` only
//! differs from SQLite's when a term is in more than one column of a row.
//!
//! The corpus is a file rather than a literal for the reason `differential.rs`
//! keeps its own: five hundred documents inline would be a file nobody reads,
//! and a corpus that can be regenerated is a corpus that can be grown.
//!
//! What it holds, and why each part is there:
//!
//! - the five hundred documents the reviewer generated, which carry the
//!   diacritics, the identifiers, the ticket numbers and the punctuation that
//!   make a tokenizer's edges visible;
//! - two rows added here, one holding `crosscolumn` in both its columns and one
//!   holding it in a single column. `bm25()` used to saturate each column
//!   separately and add the results, which agrees with SQLite exactly when no
//!   row has the term twice - so the old test's four row corpus could claim
//!   agreement "to the last digit" and was blind to the defect (task-1979, R3);
//! - the three `fts5vocab` shapes, because they read the index's own storage
//!   rather than its answers.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use inillucent_compat::differential::{compare, Step};
use inillucent_compat::workspace_root;

/// Where this suite's scratch databases live.
const AREA: &str = "fts5-parity";

/// Returns the corpus and the queries as steps the differential runs.
///
/// The lines are leaked because [`Step`] holds `&'static str` and the corpus is
/// read at run time. A test process that leaks its corpus once is the cheapest
/// correct answer; the alternative is a second `Step` type that borrows.
fn steps() -> Vec<Step> {
    let base = workspace_root().join("crates/inillucent-compat/tests/corpora/fts5-parity");
    let corpus =
        std::fs::read_to_string(base.join("corpus.sql")).expect("the corpus is in the tree");
    let queries =
        std::fs::read_to_string(base.join("queries.list")).expect("the queries are in the tree");
    let mut steps: Vec<Step> = Vec::new();
    for line in corpus.lines() {
        if line.trim().is_empty() {
            continue;
        }
        steps.push(Step::Exec(Box::leak(line.to_string().into_boxed_str())));
    }
    for line in queries.lines() {
        let Some((_, sql)) = line.split_once('\t') else {
            continue;
        };
        steps.push(Step::Query(Box::leak(sql.to_string().into_boxed_str())));
    }
    steps
}

/// Every query in the corpus answers what SQLite answers.
///
/// **Thirteen of the forty one disagreed at the commit this was written
/// against (task-1979, R4, R7, R14, R17).** `^term` was parsed and the anchor
/// thrown away, so it matched the term anywhere; `{a b}:term`, `{a}:term`,
/// `-a:term` and `"a b"*` were syntax errors here and answers there; three
/// strings SQLite calls errors were answered with rows; and `bm25()` differed
/// on every row holding a term in two columns and on every weighted call.
#[test]
fn the_corpus_answers_what_sqlite_answers() {
    let all = steps();
    let compared = compare(AREA, "parity", &all);
    if compared == 0 {
        return;
    }
    assert_eq!(compared, all.len(), "every step was compared");
}
