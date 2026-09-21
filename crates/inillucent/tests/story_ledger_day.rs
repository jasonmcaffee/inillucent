//! A day of an application's transactions, checked against a model.
//!
//! Invariant: **after three thousand transactions, with a checkpoint every two
//! hundred, a reopen every five hundred and a `VACUUM`, `REINDEX` or `ANALYZE`
//! every thousand, the database holds exactly what the statements said it
//! should** - asked through the table, through the index, through the triggers
//! that maintain a running total, and through a view over all three.
//!
//! ## Why a soak, when every statement in it already has a test
//!
//! `INSERT`, `DELETE`, `UPDATE`, a foreign key, an index, a trigger and a view
//! all have their own tests, and every one of them passes. What none of them
//! asks is what happens to the *fourteenth* page split, or to an index whose
//! entries have been rewritten two thousand times, or to a table that was
//! `VACUUM`ed while a trigger's running total was mid update. Every corruption
//! this project has fixed was in that class: a sequence of ordinary statements,
//! each of which works.
//!
//! The comparison comes from outside the engine, which is the other half. The
//! model is a `BTreeMap` in the test, so what it knows is what the statements
//! should have done rather than what the file says they did - and a corrupted
//! index, which answers consistently with itself, is caught by it.
//!
//! ## Two arms, not six
//!
//! **Not a matrix story.** The whole e2e tier has to stay under fifteen
//! seconds, and a soak at six arms is most of that on its own. The two here are the ones that differ in how
//! storage behaves: the engine's own 32,768 byte page and SQLite's 4,096 byte
//! page, which splits leaves six times as often. `matrix.rs` runs the rest of
//! the stories at all six.
//!
//! The long form is `story_ledger_day_nightly.rs`: a hundred thousand
//! transactions, and the same script replayed through the pinned SQLite shell.

use std::path::PathBuf;

use inillucent_compat::ledger::{play, Model};
use inillucent_compat::matrix::{default_arm, sqlite_page_arm, Arm, Scale};

/// The seed every run of this story uses.
///
/// A fixed seed rather than a random one, because a soak whose failure cannot
/// be replayed is a bug report nobody can act on. Every divergence prints it
/// and the transaction number, and `INILLUCENT_LEDGER_SEED` overrides it for
/// anybody hunting a second one.
const SEED: u64 = 0x1ED6_2036;

/// How many transactions the quick form issues.
///
/// **Eight hundred, where the design said three thousand, and the difference is
/// a measurement rather than a preference.** In the debug build the suite runs,
/// a transaction costs about 5.6 ms - parse, plan, write, and fire the trigger
/// that moves the account's balance - and a `VACUUM` over a populated ledger
/// costs 3.8 seconds at a thousand transactions and 0.02 seconds for the
/// `REINDEX` beside it. Three thousand transactions at two arms took 31
/// seconds, and the whole e2e tier has fifteen.
///
/// Eight hundred still crosses every phase boundary, because `Phases::for_run`
/// divides the cadence down with the run: eighty transactions between
/// checkpoints, two hundred between reopens, two maintenance statements.
///
/// The longer forms are not lost. `INILLUCENT_SCENARIO=full` runs 20,000 here,
/// and `story_ledger_day_nightly.rs` runs 100,000 with the same script replayed
/// through the pinned SQLite shell.
const TRANSACTIONS: usize = 800;

/// Returns an empty scratch directory for one arm's run.
///
/// @param arm - the arm being run
fn area(arm: &Arm) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("ledger-day")
        .join(arm.name);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns the seed to run with.
fn seed() -> u64 {
    std::env::var("INILLUCENT_LEDGER_SEED")
        .ok()
        .and_then(|named| {
            let text = named.trim().trim_start_matches("0x");
            u64::from_str_radix(text, 16).ok()
        })
        .unwrap_or(SEED)
}

/// Asserts the run did the work it claims to have done.
///
/// Rule 1.2. A generator that issued three thousand inserts and no deletes
/// would satisfy every comparison in `play`, because the comparisons check
/// agreement rather than coverage - so what the mix actually was is asserted
/// here, once, in numbers.
///
/// @param model - what the run produced
/// @param transactions - how many were issued
fn the_run_did_the_work(model: &Model, transactions: usize) {
    let [inserted, deleted, updated] = model.issued;
    assert_eq!(
        inserted.saturating_add(deleted).saturating_add(updated),
        transactions,
        "the run issued a different number of transactions than it was asked for"
    );
    assert!(
        deleted >= transactions / 20,
        "only {deleted} of {transactions} transactions were deletes, so the rows that come out \
         of the trees were barely exercised"
    );
    assert!(
        updated >= transactions / 40,
        "only {updated} of {transactions} transactions were updates"
    );
    assert!(
        model.entries.len() >= transactions / 2,
        "the ledger ended with {} entries out of {transactions} transactions, which is not the \
         growing table this story is about",
        model.entries.len()
    );
}

/// A day of transactions at the engine's own page size.
#[test]
fn a_day_of_transactions_agrees_with_the_model() {
    let arm = default_arm();
    let transactions = Scale::from_env().pick(TRANSACTIONS, 20_000);
    let model = play(
        &arm,
        &area(&arm).join("ledger.rdb"),
        transactions,
        seed(),
        None,
    );
    the_run_did_the_work(&model, transactions);
}

/// The same day at SQLite's page size, where leaves split six times as often.
///
/// The same seed, so the two runs issue the same statements: a divergence that
/// happens here and not above is about the page geometry rather than about the
/// statements, which is the difference the arm exists to make visible.
#[test]
fn a_day_of_transactions_agrees_with_the_model_at_sqlites_page_size() {
    let arm = sqlite_page_arm();
    let transactions = Scale::from_env().pick(TRANSACTIONS, 20_000);
    let model = play(
        &arm,
        &area(&arm).join("ledger.rdb"),
        transactions,
        seed(),
        None,
    );
    the_run_did_the_work(&model, transactions);
}
