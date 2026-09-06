//! The new engine's write path, graded against pinned SQLite 3.53.4.
//!
//! Invariant: after any sequence of writes, **every index still agrees with its
//! table**. That is the property this file exists for, and it is the one a
//! passing read suite proves least: an index the insert path maintains and the
//! delete path forgets answers a covering query wrongly while every other query
//! about the same row is fine, so the failure is invisible to any test that
//! reads the table.
//!
//! The sweep therefore asks each question **twice**: once in a shape the
//! planner answers from the table, and once in a shape it answers from an
//! index. A difference between those two answers is an index that has drifted,
//! and it is reported as such rather than as a query difference.
//!
//! ## Why the oracle answers over a file it built
//!
//! The write corpus needs constraints the read corpus does not have - a
//! secondary `UNIQUE` index, a `NOT NULL`, an `ON CONFLICT` target - so the
//! fixture is built here rather than checked in. Both engines then get the same
//! file: SQLite opens it, and the new engine imports it.
//!
//! When the pinned oracle has not been built this file says so and returns, the
//! way the rest of the differential suite does. A run without the oracle
//! evidences nothing and must not look like a pass.

use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// Returns the pinned SQLite oracle binary, if it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns a fresh directory for one test's files.
///
/// A directory per test rather than a file per test, so two tests running in
/// parallel never open each other's database - and so the whole of a failed
/// run's evidence is in one place.
///
/// @param tag - what to name it after
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-writes-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// The schema the write corpus is asked about.
///
/// Every constraint here is one the write path has to enforce, and each is
/// distinct: `id` is the rowid, `email` is a secondary `UNIQUE` index, `team`
/// carries a plain index, and `alias` carries a `UNIQUE` index that is
/// nullable - which is the case where SQL's "every NULL is distinct" rule
/// applies and a naive uniqueness check reports a collision that is not one.
const SCHEMA: &[&str] = &[
    "CREATE TABLE members (id INTEGER PRIMARY KEY, email TEXT NOT NULL, \
     team TEXT, alias TEXT, score INTEGER)",
    "CREATE UNIQUE INDEX members_email ON members (email)",
    "CREATE INDEX members_team ON members (team)",
    "CREATE UNIQUE INDEX members_alias ON members (alias)",
    "CREATE INDEX members_score ON members (score DESC, email)",
];

/// The rows the fixture starts with.
const SEED: &[&str] = &[
    "INSERT INTO members VALUES (1, 'ana@x', 'red', 'ana', 30)",
    "INSERT INTO members VALUES (2, 'bo@x', 'blue', NULL, 20)",
    "INSERT INTO members VALUES (3, 'cy@x', 'red', 'cy', 40)",
    "INSERT INTO members VALUES (4, 'di@x', 'green', NULL, 20)",
    "INSERT INTO members VALUES (5, 'ed@x', 'blue', 'ed', 10)",
];

/// A fixture both engines have opened, with the oracle already positioned on it.
struct Pair {
    engine: ImportedDatabase,
    oracle: Driver,
    /// Kept so the directory outlives the test.
    _directory: PathBuf,
}

/// Builds the fixture with the oracle and imports it into the new engine.
///
/// Returns `None` when the oracle has not been built, which every test here
/// reports rather than passing quietly.
///
/// @param tag - what to name the scratch directory after
fn pair(tag: &str) -> Option<Pair> {
    let program = oracle_path()?;
    let directory = scratch(tag);
    let path = directory.join("members.db");
    let mut oracle = Driver::start("sqlite", &program).expect("the oracle starts");
    oracle
        .send(&Op::Open(path.to_string_lossy().into_owned()))
        .expect("the oracle opens the fixture");
    for sql in SCHEMA.iter().chain(SEED.iter()) {
        let observed = oracle
            .send(&Op::Exec((*sql).to_string()))
            .expect("the exec runs");
        assert!(
            observed.ok,
            "the fixture did not build: {sql}: {}",
            observed.message
        );
    }
    let engine = ImportedDatabase::import(path, 4_096)
        .unwrap_or_else(|error| panic!("the fixture did not import: {:?}", error.detail()));
    Some(Pair {
        engine,
        oracle,
        _directory: directory,
    })
}

/// The questions every write is graded by.
///
/// Each pair asks the same thing twice: once in a shape the planner answers
/// from the table, and once in a shape it answers from an index. An index that
/// has drifted from its table answers the second differently from the first,
/// and no query that only reads the table can see it.
fn probes() -> Vec<String> {
    let mut queries = vec![
        // The table, whole and in key order.
        "SELECT id, email, team, alias, score FROM members ORDER BY id".to_string(),
        "SELECT count(*) FROM members".to_string(),
        // `members_email`: a covering scan and a point lookup through it.
        "SELECT email FROM members ORDER BY email".to_string(),
        "SELECT id FROM members WHERE email = 'cy@x'".to_string(),
        "SELECT count(*) FROM members WHERE email > 'b' AND email < 'e'".to_string(),
        // `members_team`, which is not unique and holds NULLs.
        "SELECT team FROM members ORDER BY team".to_string(),
        "SELECT id FROM members WHERE team = 'red' ORDER BY id".to_string(),
        "SELECT count(*) FROM members WHERE team IS NULL".to_string(),
        // `members_alias`, unique and nullable - the case where SQL's rule that
        // every NULL is distinct means several rows share the "same" key.
        "SELECT alias FROM members ORDER BY alias".to_string(),
        "SELECT id FROM members WHERE alias = 'ed'".to_string(),
        "SELECT count(*) FROM members WHERE alias IS NULL".to_string(),
        // `members_score`, whose leading column is stored descending.
        "SELECT score, email FROM members ORDER BY score DESC, email".to_string(),
        "SELECT id FROM members WHERE score = 20 ORDER BY id".to_string(),
        "SELECT count(*) FROM members WHERE score >= 20".to_string(),
        // The aggregate every index has to agree about.
        "SELECT team, count(*), sum(score) FROM members GROUP BY team ORDER BY team".to_string(),
    ];
    queries.sort();
    queries.dedup();
    queries
}

/// Renders one of the new engine's rows the way the oracle renders its own.
///
/// @param row - the row
fn render(row: &[OwnedDatum]) -> Vec<TaggedValue> {
    row.iter()
        .map(|value| match value {
            OwnedDatum::Null => TaggedValue::Null,
            OwnedDatum::Int(number) => TaggedValue::Integer(*number),
            OwnedDatum::Real(number) => TaggedValue::Real(*number),
            OwnedDatum::Text(bytes) => TaggedValue::Text(bytes.clone()),
            OwnedDatum::Blob(bytes) => TaggedValue::Blob(bytes.clone()),
        })
        .collect()
}

/// Asks both engines every probe and returns the differences.
///
/// @param pair - the two engines over the same data
fn differences(pair: &mut Pair) -> Vec<String> {
    let mut found = Vec::new();
    for sql in probes() {
        let reference = pair
            .oracle
            .send(&Op::Query(sql.clone()))
            .expect("the oracle answers");
        if !reference.ok {
            continue;
        }
        let answered = match pair.engine.execute_any(&sql, &Params::new()) {
            Ok(outcome) => outcome.rows,
            Err(error) => {
                found.push(format!(
                    "{sql}\n  the new engine refused: {}",
                    error.detail().unwrap_or("no detail")
                ));
                continue;
            }
        };
        let ours: Vec<Vec<TaggedValue>> = answered.iter().map(|row| render(row)).collect();
        if !same(&reference.rows, &ours) {
            found.push(format!(
                "{sql}\n  sqlite {:?}\n  ours   {:?}",
                reference.rows, ours
            ));
        }
    }
    found
}

/// Compares two row sequences by value and by type.
///
/// @param reference - what SQLite answered
/// @param ours - what the new engine answered
fn same(reference: &[Vec<TaggedValue>], ours: &[Vec<TaggedValue>]) -> bool {
    reference.len() == ours.len()
        && reference.iter().zip(ours.iter()).all(|(left, right)| {
            left.len() == right.len() && left.iter().zip(right.iter()).all(|(a, b)| a.identical(b))
        })
}

/// Applies one statement to both engines and returns what each said.
///
/// The answer is the error message, or `None` for a statement that worked. Both
/// halves matter: a statement one engine refuses and the other accepts is a
/// difference even when the resulting rows happen to agree.
///
/// @param pair - the two engines over the same data
/// @param sql - the statement
fn apply(pair: &mut Pair, sql: &str) -> (Option<String>, Option<String>) {
    let reference = pair
        .oracle
        .send(&Op::Exec(sql.to_string()))
        .expect("the oracle runs the statement");
    let ours = pair
        .engine
        .execute_any(sql, &Params::new())
        .err()
        .map(|error| error.message().to_string());
    let theirs = (!reference.ok).then(|| reference.message.clone());
    (theirs, ours)
}

/// Every index still agrees with its table after a campaign of writes.
///
/// The campaign is written out rather than generated because each statement is
/// here for a reason the next one is not: a plain insert, an insert that lands
/// between two existing keys, an update that moves a row in one index and not
/// another, an update that moves it in all of them, a delete of a row several
/// indexes name, and a re-insert of the key that was just freed.
#[test]
fn every_index_still_agrees_with_its_table_after_a_campaign() {
    let Some(mut pair) = pair("campaign") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let campaign = [
        "INSERT INTO members VALUES (6, 'fi@x', 'red', 'fi', 50)",
        "INSERT INTO members VALUES (7, 'gu@x', NULL, NULL, 20)",
        // An update that moves the row in `members_team` and nowhere else.
        "UPDATE members SET team = 'blue' WHERE id = 1",
        // An update that moves it in `members_email`, `members_alias` and
        // `members_score` at once.
        "UPDATE members SET email = 'zed@x', alias = 'zed', score = 5 WHERE id = 3",
        // An update to the value it already holds, which must remove and re-add
        // one entry rather than leave two.
        "UPDATE members SET team = 'blue' WHERE id = 2",
        // A delete of a row three indexes name.
        "DELETE FROM members WHERE id = 5",
        // The key that was just freed, taken again.
        "INSERT INTO members VALUES (5, 'new@x', 'green', 'new', 15)",
        // A delete that removes nothing, which must leave every index alone.
        "DELETE FROM members WHERE id = 999",
        // An update over a range rather than a key.
        "UPDATE members SET score = score + 1 WHERE team = 'blue'",
        // A delete over an index rather than the table.
        "DELETE FROM members WHERE alias IS NULL",
    ];

    let mut failures = Vec::new();
    for sql in campaign {
        let (reference, ours) = apply(&mut pair, sql);
        if reference.is_some() != ours.is_some() {
            failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {ours:?}"));
            continue;
        }
        // The structure, after every statement rather than at the end. A tree
        // whose separators no longer bound its children answers correctly for a
        // long time, so the check has to be a check - and running it per
        // statement is what names the statement that broke it.
        if let Err(error) = pair.engine.check_trees() {
            failures.push(format!(
                "after {sql}\n  a tree is structurally wrong: {}",
                error.detail().unwrap_or("no detail")
            ));
        }
        for difference in differences(&mut pair) {
            failures.push(format!("after {sql}\n{difference}"));
        }
    }
    assert!(
        failures.is_empty(),
        "the write path and SQLite disagree:\n{}",
        failures.join("\n\n")
    );
}

/// A duplicate key reports SQLite's own text, for every kind of key.
///
/// The acceptance asks for SQLite's `UNIQUE` error rather than merely an error,
/// which means the *text*: a caller matching on `UNIQUE constraint failed:
/// members.email` must not have to know which engine answered it.
#[test]
fn a_duplicate_key_reports_the_text_sqlite_reports() {
    let Some(mut pair) = pair("unique") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let collisions = [
        // The rowid, aliased by an `INTEGER PRIMARY KEY`.
        "INSERT INTO members VALUES (1, 'zz@x', 'red', 'zz', 1)",
        // A secondary `UNIQUE` index.
        "INSERT INTO members VALUES (80, 'ana@x', 'red', 'zz', 1)",
        // A second secondary `UNIQUE` index, so the message names the right one.
        "INSERT INTO members VALUES (81, 'zz@x', 'red', 'ana', 1)",
    ];

    let mut failures = Vec::new();
    for sql in collisions {
        let (reference, ours) = apply(&mut pair, sql);
        let Some(reference) = reference else {
            failures.push(format!(
                "{sql}\n  sqlite accepted it and we expected a refusal"
            ));
            continue;
        };
        match ours {
            Some(message) if message == reference => {}
            other => failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {other:?}")),
        }
    }
    assert!(
        failures.is_empty(),
        "the constraint messages differ from SQLite's:\n{}",
        failures.join("\n\n")
    );
}

/// A nullable `UNIQUE` column holds any number of NULLs.
///
/// SQL's rule is that a NULL is distinct from every other NULL, so
/// `members_alias` may name four rows with no alias and none of them collides.
/// A uniqueness check that probed the index without excluding NULLs would
/// refuse the second one - and the fixture already has two, so the bug would
/// show up as an insert that cannot happen rather than as a wrong answer.
#[test]
fn a_null_never_collides_in_a_unique_index() {
    let Some(mut pair) = pair("nulls") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let mut failures = Vec::new();
    for sql in [
        "INSERT INTO members VALUES (20, 'p@x', 'red', NULL, 1)",
        "INSERT INTO members VALUES (21, 'q@x', 'red', NULL, 1)",
        "UPDATE members SET alias = NULL WHERE id = 1",
    ] {
        let (reference, ours) = apply(&mut pair, sql);
        if reference.is_some() || ours.is_some() {
            failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {ours:?}"));
        }
    }
    for difference in differences(&mut pair) {
        failures.push(difference);
    }
    assert!(
        failures.is_empty(),
        "a NULL was treated as a value in a unique index:\n{}",
        failures.join("\n\n")
    );
}

/// `ON CONFLICT` resolves a collision the way SQLite resolves it.
#[test]
fn on_conflict_does_what_sqlite_does() {
    let Some(mut pair) = pair("conflict") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let mut failures = Vec::new();
    for sql in [
        // `DO NOTHING`: the row already there is left exactly as it was.
        "INSERT INTO members VALUES (1, 'zz@x', 'gold', 'zz', 99) ON CONFLICT DO NOTHING",
        // `DO UPDATE`, reading `excluded`.
        "INSERT INTO members VALUES (2, 'bo@x', 'gold', 'bo2', 77) \
         ON CONFLICT(id) DO UPDATE SET score = excluded.score, team = excluded.team",
        // `OR IGNORE`, the statement-level form.
        "INSERT OR IGNORE INTO members VALUES (3, 'zz@x', 'gold', 'zz', 99)",
        // `OR REPLACE`, which deletes the row in the way and takes its place.
        "INSERT OR REPLACE INTO members VALUES (4, 'zz@x', 'gold', 'zz', 99)",
        // An upsert that has to find the conflict through a secondary index.
        "INSERT INTO members VALUES (90, 'ana@x', 'gold', 'a90', 5) \
         ON CONFLICT(email) DO UPDATE SET score = excluded.score",
    ] {
        let (reference, ours) = apply(&mut pair, sql);
        if reference.is_some() != ours.is_some() {
            failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {ours:?}"));
            continue;
        }
        for difference in differences(&mut pair) {
            failures.push(format!("after {sql}\n{difference}"));
        }
    }
    assert!(
        failures.is_empty(),
        "`ON CONFLICT` resolved differently from SQLite:\n{}",
        failures.join("\n\n")
    );
}

/// A rowid the statement leaves out is the next one after the largest.
#[test]
fn an_omitted_rowid_is_allocated_the_way_sqlite_allocates_it() {
    let Some(mut pair) = pair("rowid") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let mut failures = Vec::new();
    for sql in [
        "INSERT INTO members (email, team, alias, score) VALUES ('n1@x', 'red', 'n1', 1)",
        "INSERT INTO members (email, team, alias, score) VALUES ('n2@x', 'red', 'n2', 2)",
        // The largest key is deleted, so the next allocation reuses its number -
        // which is SQLite's rule for a table that is not `AUTOINCREMENT`. The
        // key is written out rather than found with `max(id)`, because a scalar
        // subquery in a `WHERE` is a read-path gap this phase leaves named and
        // open, and a test that met it here would be grading the write path on
        // somebody else's refusal.
        "DELETE FROM members WHERE id = 7",
        "INSERT INTO members (email, team, alias, score) VALUES ('n3@x', 'red', 'n3', 3)",
        // And once more, so the reused number is followed by a fresh one.
        "INSERT INTO members (email, team, alias, score) VALUES ('n4@x', 'red', 'n4', 4)",
    ] {
        let (reference, ours) = apply(&mut pair, sql);
        if reference.is_some() != ours.is_some() {
            failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {ours:?}"));
            continue;
        }
        for difference in differences(&mut pair) {
            failures.push(format!("after {sql}\n{difference}"));
        }
    }
    assert!(
        failures.is_empty(),
        "rowids were allocated differently from SQLite:\n{}",
        failures.join("\n\n")
    );
}

/// `RETURNING` names the row as it ended up, not as it arrived.
#[test]
fn returning_names_the_row_that_was_written() {
    let Some(mut pair) = pair("returning") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let mut failures = Vec::new();
    for sql in [
        "INSERT INTO members VALUES (30, 'r1@x', 'red', 'r1', 3) RETURNING id, email",
        "UPDATE members SET score = score * 2 WHERE id = 30 RETURNING id, score",
        "DELETE FROM members WHERE id = 30 RETURNING id, email",
    ] {
        let reference = pair
            .oracle
            .send(&Op::Query(sql.to_string()))
            .expect("the oracle answers");
        let ours = match pair.engine.execute_any(sql, &Params::new()) {
            Ok(outcome) => outcome.rows,
            Err(error) => {
                failures.push(format!(
                    "{sql}\n  the new engine refused: {}",
                    error.detail().unwrap_or("no detail")
                ));
                continue;
            }
        };
        let rendered: Vec<Vec<TaggedValue>> = ours.iter().map(|row| render(row)).collect();
        if !same(&reference.rows, &rendered) {
            failures.push(format!(
                "{sql}\n  sqlite {:?}\n  ours   {rendered:?}",
                reference.rows
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "`RETURNING` answered differently from SQLite:\n{}",
        failures.join("\n\n")
    );
}

/// Every imported tree is in its own key order.
///
/// A descending index is stored descending by SQLite, and the import walks it
/// in that physical order. A tree whose leaves are not in the order its own
/// comparisons define answers a *seek* wrongly while a scan of it looks fine -
/// so this is checked directly rather than inferred from a query.
#[test]
fn every_imported_tree_is_in_its_own_key_order() {
    let Some(pair) = pair("order") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };
    pair.engine
        .check_trees()
        .unwrap_or_else(|error| panic!("a tree is out of order: {:?}", error.detail()));
}

/// The two engines agree about the fixture before anything is written to it.
///
/// The baseline every other test in this file rests on. A difference that is
/// already there before the first write is an import or a read difference, and
/// grading a write against it would blame the write path for something it did
/// not do.
#[test]
fn the_two_engines_agree_before_anything_is_written() {
    let Some(mut pair) = pair("baseline") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };
    let found = differences(&mut pair);
    assert!(
        found.is_empty(),
        "the engines disagree about the fixture as imported:\n{}",
        found.join("\n\n")
    );
}

/// A scalar subquery in a `WHERE` is refused by name, on reads and on writes.
///
/// It is a **read**-path gap rather than a write one: nothing in the physical
/// pass translates a `BoundExpr::Subquery` in an expression position, so a
/// query with one is refused whether or not a write is attached to it. It is
/// recorded here because the write path is where it was met, and because the
/// refusal has to name the construct rather than fail as a misuse - which is
/// the whole invariant `inillucent-exec`'s physical pass is written to.
#[test]
fn a_scalar_subquery_in_a_where_is_refused_by_name() {
    let Some(mut pair) = pair("subquery") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };
    for sql in [
        "SELECT id FROM members WHERE id = (SELECT max(id) FROM members)",
        "DELETE FROM members WHERE id = (SELECT max(id) FROM members)",
        "UPDATE members SET score = 0 WHERE id = (SELECT max(id) FROM members)",
    ] {
        let error = pair
            .engine
            .execute_any(sql, &Params::new())
            .err()
            .unwrap_or_else(|| panic!("{sql} was accepted and the refusal is what is asserted"));
        let detail = error.detail().unwrap_or("no detail").to_ascii_lowercase();
        assert!(
            detail.contains("subquery"),
            "{sql} was refused without naming the construct: {detail}"
        );
    }
}
