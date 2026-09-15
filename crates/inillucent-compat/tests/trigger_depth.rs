//! How deep a chain of triggers may go, against the oracle and against `.limit`.
//!
//! Invariant: **the trigger depth cap is one number, it is the one `.limit`
//! reports, and the refusal says what it was.**
//!
//! Two constants of the name `MAX_TRIGGER_DEPTH` used to exist (task-1946, H3).
//! `crates/inillucent-sql/src/bind.rs` declared 32 and enforced it where trigger
//! bodies are inlined, which is the number a user actually hit.
//! `crates/inillucent-exec/src/trigger.rs` declared 1000 and checked it at run
//! time - over a tree the binder had already capped at 32, so that check could
//! never fire. Meanwhile `crates/inillucent-base/manifests/limits.toml`
//! advertised 1000 and `inillucent diagnose` printed 1000.
//!
//! So the engine refused at 32 while every place a user could read the number
//! said 1000, and the refusal said only `too many levels of trigger recursion`
//! with no number in it at all. A chain of forty distinct triggers - which the
//! oracle runs without complaint - was refused at bind time.
//!
//! A self-referencing trigger is a different case and is not affected: the
//! binder skips a trigger already on its own firing stack (`dml.rs`,
//! `firing.contains`), which is SQLite's behaviour with
//! `recursive_triggers = off` and is also what makes the inlining terminate.
//! Only chains of *distinct* triggers reach the cap.

use inillucent_compat::differential::{self, Step};

/// How many tables the chain is built from.
///
/// Forty, because the old cap was 32: a chain of forty is past it and a chain
/// of thirty would have passed either way.
const CHAIN: usize = 40;

/// Returns the schema and the write that drives a chain of `length` triggers.
///
/// `t0` to `t{length-1}`, each with an `AFTER INSERT` trigger inserting into
/// the next, so one row written into `t0` walks the whole chain. The statements
/// are leaked because `Step` holds `&'static str` and these are built at run
/// time; a test process that ends is the collection.
///
/// @param length - how many tables, and therefore how deep the chain runs
fn chain(length: usize) -> Vec<Step> {
    let mut steps = Vec::new();
    for table in 0..length {
        let ddl: &'static str =
            Box::leak(format!("CREATE TABLE t{table}(a INTEGER)").into_boxed_str());
        steps.push(Step::Exec(ddl));
    }
    for table in 0..length.saturating_sub(1) {
        let trigger: &'static str = Box::leak(
            format!(
                "CREATE TRIGGER fire{table} AFTER INSERT ON t{table} \
                 BEGIN INSERT INTO t{} VALUES (NEW.a); END",
                table + 1
            )
            .into_boxed_str(),
        );
        steps.push(Step::Exec(trigger));
    }
    steps.push(Step::Exec("INSERT INTO t0 VALUES (1)"));
    for table in 0..length {
        let query: &'static str =
            Box::leak(format!("SELECT count(*) FROM t{table}").into_boxed_str());
        steps.push(Step::Query(query));
    }
    steps
}

/// A chain of forty distinct triggers matches the oracle.
///
/// Every table's count is compared, not only the first: a chain that stopped
/// half way would leave `t0` with its row and `t39` empty, and asserting on the
/// write alone would call that a pass.
#[test]
fn a_forty_deep_chain_of_triggers_matches_sqlite() {
    let steps = chain(CHAIN);
    let expected = steps.len();
    let compared = differential::compare("trigger_depth", "forty-deep-chain", &steps);
    assert!(
        compared == 0 || compared == expected,
        "compared {compared} of {expected} steps"
    );
}

/// Returns how many rows one table holds.
///
/// @param connection - the open connection
/// @param table - the table's name
fn count_of(connection: &inillucent_engine::connect::Connection<'_>, table: &str) -> i64 {
    let rows = connection
        .query(&format!("SELECT count(*) FROM {table}"))
        .unwrap_or_else(|why| panic!("counting {table}: {}", why.message()));
    match rows.first().and_then(|row| row.first()) {
        Some(inillucent_tree::datum::OwnedDatum::Int(count)) => *count,
        other => panic!("counting {table} gave {other:?}"),
    }
}

/// `.limit trigger_depth 10` is what the binder then enforces, and the refusal
/// names the ten.
///
/// **This is the half the differential cannot ask.** The oracle has its own
/// compiled-in limit, so a comparison can only show that the two agree at the
/// default; what a settable limit means is that a different number takes effect
/// and is reported, and only this engine can be asked that.
#[test]
fn the_configured_limit_is_the_one_enforced_and_the_one_named() {
    let directory = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("trigger-depth");
    let _ = std::fs::create_dir_all(&directory);
    let path = directory.join("configured.rdb");
    let _ = std::fs::remove_file(&path);

    let database = inillucent_engine::connect::Database::open(&path).expect("the database opens");
    let connection = database.session();

    // Twelve tables and eleven triggers, so the chain is one level deeper than
    // the limit about to be set: the binder refuses when the eleventh trigger
    // would make `firing` eleven long against a cap of ten.
    for table in 0..12 {
        connection
            .execute_batch(&format!("CREATE TABLE t{table}(a INTEGER)"))
            .expect("the table is created");
    }
    for table in 0..11 {
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER fire{table} AFTER INSERT ON t{table} \
                 BEGIN INSERT INTO t{} VALUES (NEW.a); END",
                table + 1
            ))
            .expect("the trigger is created");
    }

    // At the default of 1000 the chain binds and runs.
    assert_eq!(
        database.limit(inillucent_base::limits::Limit::TriggerDepth),
        1000
    );
    connection
        .execute_batch("INSERT INTO t0 VALUES (1)")
        .expect("an eleven deep chain runs under a limit of 1000");
    assert_eq!(
        count_of(&connection, "t11"),
        1,
        "the chain did not reach the last table"
    );

    // Lowering it to ten refuses the eleventh level, and says ten.
    database.set_limit(inillucent_base::limits::Limit::TriggerDepth, 10);
    assert_eq!(
        database.limit(inillucent_base::limits::Limit::TriggerDepth),
        10
    );
    let refusal = connection
        .execute_batch("INSERT INTO t0 VALUES (2)")
        .expect_err("an eleven deep chain is past a limit of ten");
    let message = refusal.message().to_string();
    assert!(
        message.contains("too many levels of trigger recursion"),
        "{message}"
    );
    assert!(
        message.contains("10"),
        "the refusal does not name the limit that was in force: {message}"
    );

    // And raising it again lets the same write through, so the number is being
    // read per statement rather than baked in when the connection opened.
    database.set_limit(inillucent_base::limits::Limit::TriggerDepth, 1000);
    connection
        .execute_batch("INSERT INTO t0 VALUES (3)")
        .expect("the chain runs again once the limit is raised");
}
