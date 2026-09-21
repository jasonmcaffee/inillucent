//! The long form of the ledger day: a hundred thousand transactions, and the
//! same statements replayed through the pinned SQLite shell.
//!
//! Invariant: **after a hundred thousand transactions the database holds
//! exactly what the statements said it should, and so does SQLite when it is
//! given the same statements.** The model is the first answer and the pinned
//! shell is the second, and the two are independent of each other and of this
//! engine.
//!
//! ## What the long form finds that the short one cannot
//!
//! `story_ledger_day.rs` issues eight hundred transactions, which is one page
//! split of the table and a handful of the index. A hundred thousand is a tree
//! with interior pages that have themselves split, a free list that has been
//! reused, an index whose entries have been rewritten tens of thousands of
//! times, and a log that has been checkpointed two hundred times. None of that
//! is reachable in fifteen seconds, which is why this is `nightly` rather than
//! a bigger number in the other file.
//!
//! ## One arm, and a cadence of its own
//!
//! **Not a matrix story.** It runs at `default_arm` alone: a hundred thousand
//! transactions is an hour, and six of them is a night spent on one question.
//! The page sizes are compared in the short form, which runs at two.
//!
//! The cadence is stated rather than derived. `Phases::for_run` would
//! checkpoint every 200 and run `VACUUM`, `REINDEX` or
//! `ANALYZE` every 1,000 - a hundred maintenance statements over a table
//! growing to seventy thousand rows. A `VACUUM` costs 3.8 seconds at a thousand
//! transactions in the debug build the suite runs and grows with the table, so
//! that run would spend its night on maintenance rather than on transactions.
//! The cadence here is stated rather than derived, and it still crosses every
//! boundary: a hundred checkpoints, twenty reopens, four maintenance
//! statements.
//!
//! ## The oracle
//!
//! `play` records every statement it issued. SQLite is given that file and
//! asked the same three questions. It is a third answer rather than a second:
//! a divergence between this engine and the model says the engine is wrong, and
//! a divergence between the model and SQLite would say the model is.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::differential::skipping;
use inillucent_compat::ledger::{play_with, Model, Phases};
use inillucent_compat::matrix::default_arm;
use inillucent_compat::workspace_root;

/// The seed the long run uses, which is the short run's seed.
///
/// The same statements, so a divergence that happens here and not in
/// `story_ledger_day.rs` is about the size of the run rather than about what it
/// issued.
const SEED: u64 = 0x1ED6_2036;

/// How many transactions the long form issues.
const TRANSACTIONS: usize = 100_000;

/// How often the long run checkpoints, reopens and runs maintenance.
const CADENCE: Phases = Phases {
    checkpoint: 1_000,
    reopen: 5_000,
    maintenance: 25_000,
};

/// Returns an empty scratch directory.
fn area() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ledger-day-nightly");
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns the pinned SQLite shell, if it has been built.
fn reference() -> Option<PathBuf> {
    let path = workspace_root()
        .join(".sqlite-ref/3.53.4/shell")
        .join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Asks the pinned shell one question and returns what it printed.
///
/// The separator is a comma because `stories::line` renders a row with commas,
/// and what both answers are compared against is the model's own rendering.
///
/// @param shell - the pinned `sqlite3`
/// @param database - the file to ask
/// @param sql - the statement
fn ask_sqlite(shell: &Path, database: &Path, sql: &str) -> String {
    let output = Command::new(shell)
        .arg("-cmd")
        .arg(".mode list")
        .arg("-cmd")
        .arg(".separator ,")
        .arg("-cmd")
        .arg(".headers off")
        .arg(database)
        .arg(sql)
        .output()
        .unwrap_or_else(|why| panic!("the pinned shell would not run: {why}"));
    assert!(
        output.status.success(),
        "the pinned shell refused `{sql}`: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .replace("\r\n", "\n")
}

/// Asserts the run issued the mix it claims to have issued.
///
/// @param model - what the run produced
fn the_run_did_the_work(model: &Model) {
    let [inserted, deleted, updated] = model.issued;
    assert_eq!(
        inserted.saturating_add(deleted).saturating_add(updated),
        TRANSACTIONS,
        "the run issued a different number of transactions than it was asked for"
    );
    assert!(
        deleted >= TRANSACTIONS / 20 && updated >= TRANSACTIONS / 40,
        "the mix was {inserted} inserts, {deleted} deletes and {updated} updates, which is not \
         the mix this story is about"
    );
    assert!(
        model.entries.len() >= 40_000,
        "the ledger ended with {} entries, which is not the large tree this story is about",
        model.entries.len()
    );
}

/// A hundred thousand transactions, checked against the model and against
/// SQLite.
#[test]
fn a_hundred_thousand_transactions_agree_with_the_model_and_with_sqlite() {
    let area = area();
    let script = area.join("day.sql");
    let model = play_with(
        &default_arm(),
        &area.join("ledger.rdb"),
        TRANSACTIONS,
        SEED,
        Some(&script),
        CADENCE,
    );
    the_run_did_the_work(&model);

    let Some(shell) = reference() else {
        skipping(
            "the pinned SQLite oracle is not built, so the replay half of this story did not \
             run; build it with tools/sqlite-reference.ps1",
        );
        return;
    };

    // The same statements, in the same order, to a different engine. `VACUUM`
    // is not among them: it changes how rows are stored and nothing about what
    // they are, and what is being compared here is what they are.
    let replay = area.join("replay.db");
    let output = Command::new(&shell)
        .arg(&replay)
        .arg(format!(".read {}", script.display()))
        .output()
        .unwrap_or_else(|why| panic!("the pinned shell would not run: {why}"));
    assert!(
        output.status.success(),
        "the pinned shell refused the replay: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        ask_sqlite(
            &shell,
            &replay,
            "SELECT id, account_id, amount FROM entry ORDER BY id"
        ),
        model.every_entry(),
        "SQLite, given the same statements, holds different rows from the model - which means \
         the model is wrong rather than the engine"
    );
    assert_eq!(
        ask_sqlite(
            &shell,
            &replay,
            "SELECT id, balance FROM account ORDER BY id"
        ),
        model.every_balance(),
        "SQLite's triggers left different balances from the model's"
    );
    assert_eq!(
        ask_sqlite(&shell, &replay, "SELECT count(*) FROM entry"),
        model.entries.len().to_string(),
        "SQLite holds a different number of entries"
    );
}
