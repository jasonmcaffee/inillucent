//! Every statement Nikaya prepares, prepared and run here, at every arm.
//!
//! Invariant: **a statement the consumer's source contains is a statement this
//! engine accepts, or its refusal is written down in `allow.list` with a ticket
//! key.** Nothing about the rows: what this asserts is that the statement
//! compiles and runs, because that is the failure the escape it is shaped
//! around was.
//!
//! ## The escape
//!
//! `2f820f3`. A compound `SELECT` used as a derived table was refused by the
//! binder, and Nikaya's document view answered HTTP 500. No data would have
//! found it - the statement never compiled - and no per-construct test did
//! either, because a compound `SELECT` works and a derived table works and
//! nobody had written one inside the other. What was missing was the
//! consumer's own statement.
//!
//! So the corpus is the consumer's. `tests/workloads/nikaya/statements.sql`
//! holds every SQL literal in `C:/jason/dev/nikaya/server/src`, extracted by
//! `tools/extract-nikaya-workload.py`, with the schema its four migrations
//! build and one parameter value per placeholder. Nikaya's data is private mail
//! and none of it is here.
//!
//! ## What an allow list entry means
//!
//! The same thing it means in `differential_part8.rs`: this statement does not
//! work yet, here is the ticket, and **a statement that starts working fails
//! this test until its entry is removed.** A list that only grows is a list of
//! things nobody will ever take off it.
//!
//! ## Why it runs at every arm
//!
//! Because a statement that compiles is not a statement that runs: a `SELECT`
//! over a table whose rows are wider than a 4,096 byte page takes a different
//! read path from the same `SELECT` at 32,768, and task-2033 is what that
//! difference costs when nothing exercises it.

use std::collections::BTreeMap;
use std::path::Path;

use inillucent_compat::matrix::Arm;
use inillucent_compat::nikaya::{seed_the_workload, workload};
use inillucent_compat::scenario;
use inillucent_compat::stories::{open, reopen_and_check, run, Params};
use inillucent_compat::workspace_root;
use inillucent_tree::datum::OwnedDatum;

/// Reads the allow list: the statements that do not work yet, and their ticket.
fn allowed() -> BTreeMap<String, String> {
    let path = workspace_root().join("tests/workloads/nikaya/allow.list");
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return out;
    };
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let Some((source, said)) = line.split_once('\t') else {
            panic!("{}: an allow line has no tab: {line}", path.display());
        };
        out.insert(source.trim().to_string(), said.trim().to_string());
    }
    out
}

/// Binds a statement's values into the engine's parameter set.
///
/// @param values - the values from the workload file
fn bind(values: &[OwnedDatum]) -> Params {
    let mut params = Params::new();
    for (index, value) in values.iter().enumerate() {
        let slot = u32::try_from(index + 1).unwrap_or(u32::MAX);
        params.set(slot, value.clone());
    }
    params
}

/// Every statement Nikaya prepares is one this engine accepts.
///
/// The `chunk_embedding.embedding` column is `VECTOR(768)` in Nikaya and
/// `VECTOR(4)` here, because the width is a property of the model rather than
/// of the statement and 768 floats a row is a fixture rather than a seed. Every
/// statement that names the column names it by name, so the width does not
/// reach the SQL.
fn every_statement_the_consumer_prepares_runs(arm: &Arm, area: &Path) {
    let path = area.join("workload.rdb");
    let (schema, statements) = workload();
    let allow = allowed();

    let database = open(arm, &path);
    let connection = database.session();
    // `VECTOR(768)` is Nikaya's; four is enough for the column to be the type
    // it is, and the seed rows carry a four wide value.
    run(&connection, &schema.replace("VECTOR(768)", "VECTOR(4)"));
    seed_the_workload(&connection);

    // **Two questions, and only the first one is about the engine.**
    //
    // *Does it compile?* That is what `2f820f3` failed: the binder declined a
    // compound `SELECT` used as a derived table, and no value would have
    // changed the answer. A refusal here is a failure.
    //
    // *Does it run?* The values are generated, not Nikaya's, so a statement can
    // be perfectly good and still be refused because `row-0001` is already in
    // the table it is being inserted into, or is not one of the five strings a
    // `CHECK` allows. Those refusals are about the seed and the schema, and
    // counting them as failures would mean writing a row per statement that
    // satisfies every constraint Nikaya has - which is Nikaya's fixtures, which
    // is its private mail. So a `Constraint` refusal is an outcome and anything
    // else is a failure.
    let mut compiled = 0usize;
    let mut answered = 0usize;
    let mut declined_by_a_constraint = 0usize;
    let mut refused: Vec<String> = Vec::new();
    let mut working_after_all: Vec<String> = Vec::new();
    for statement in &statements {
        let listed = allow.contains_key(&statement.source);
        let complaint = match connection.prepare(&statement.sql) {
            Ok(_) => {
                compiled += 1;
                let params = bind(&statement.params);
                match connection.query_with(&statement.sql, &params) {
                    Ok(_) => {
                        answered += 1;
                        None
                    }
                    Err(why) if why.code() == inillucent_base::PrimaryCode::Constraint => {
                        declined_by_a_constraint += 1;
                        None
                    }
                    Err(why) => Some(format!("running it: {} ({:?})", why.message(), why.code())),
                }
            }
            Err(why) => Some(format!(
                "preparing it: {} ({:?})",
                why.message(),
                why.code()
            )),
        };
        match (complaint, listed) {
            (None, true) => working_after_all.push(statement.source.clone()),
            (None, false) => {}
            (Some(_), true) => {}
            (Some(said), false) => refused.push(format!(
                "{}: {said}\n    {}",
                statement.source,
                statement.sql.replace('\n', "\n    ")
            )),
        }
    }

    assert!(
        refused.is_empty(),
        "these statements are in Nikaya's source and this engine will not take them at the {} \
         arm. A consumer's statement that does not compile is an HTTP 500 - `2f820f3` was \
         exactly this. Fix it, or add its source to tests/workloads/nikaya/allow.list with the \
         ticket that will:\n  {}",
        arm.name,
        refused.join("\n  ")
    );
    assert!(
        working_after_all.is_empty(),
        "these statements are in tests/workloads/nikaya/allow.list and now work at the {} arm, \
         so the entry has to go - a fixed defect left listed reads as coverage and is not:\n  {}",
        arm.name,
        working_after_all.join("\n  ")
    );
    assert_eq!(
        compiled,
        statements.len() - allow.len(),
        "{compiled} of {} statements compiled at the {} arm",
        statements.len(),
        arm.name
    );
    // Rule 1.2: a run where nothing answered would satisfy every assertion
    // above, because "no statement was refused" is true of a loop that refused
    // to run anything. Two thirds of Nikaya's corpus is `SELECT`s, so most of
    // it answers.
    assert!(
        answered * 2 > statements.len(),
        "only {answered} of {} statements answered at the {} arm, with \
         {declined_by_a_constraint} declined by a constraint - so the seed is not building the \
         rows the corpus reads and this is a weaker test than it reports being",
        statements.len(),
        arm.name
    );

    // The corpus is still sound after every statement in it has run, and the
    // seed rows are still there: a statement corpus that quietly deleted its
    // own rows would make every later statement match nothing.
    drop(database);
    let database = reopen_and_check(arm, &path);
    let connection = database.session();
    let remaining = connection
        .query("SELECT count(*) FROM source_account")
        .expect("the seed table reads back");
    assert_eq!(
        remaining.first().and_then(|row| row.first()),
        Some(&OwnedDatum::Int(1)),
        "the workload left the seed table with something other than its one row, at the {} arm",
        arm.name
    );
}

scenario!(
    every_statement_the_consumer_prepares_runs,
    every_statement_the_consumer_prepares_runs
);
